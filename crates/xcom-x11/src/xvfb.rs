//! Virtual display lifecycle: spawn and own an `Xvfb` instance.
//!
//! Xvfb is the one external dependency of the server half (see the M0 report
//! for the vendored-server upgrade path). Everything else is pure Rust.
//!
//! The display number is picked by Xvfb itself (`-displayfd`): scanning
//! /tmp/.X11-unix for free numbers races between concurrent sessions, and the
//! loser ends up attached to the winner's X server — which then dies with the
//! winner's daemon (seen as `shm_get_image: Broken pipe` mid-capture).
//!
//! Access control is a per-run MIT-MAGIC-COOKIE-1 file (`-auth`), not `-ac`:
//! on a multi-user host the unix socket is otherwise readable and injectable
//! by any local user.

use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::FromRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, PropMode};

pub struct Xvfb {
    pub display: String,
    pub log: PathBuf,
    auth: PathBuf,
    child: Child,
    /// Keep the `-displayfd` pipe open: Xvfb writes again when it retries a
    /// display, and a closed read end sends SIGPIPE (killing the server right
    /// after it looked ready).
    _displayfd: std::fs::File,
}

impl Xvfb {
    /// Spawn on a free display number chosen by the X server itself.
    pub fn spawn(width: u16, height: u16) -> Result<Xvfb> {
        // SIGKILLed servers leave their unix socket behind; a dying server can
        // still answer for a moment while the next Xvfb picks the same number
        // and fails to bind. Clean up and retry on a fresh display.
        let mut last = None;
        for _ in 0..3 {
            remove_stale_sockets();
            match Self::spawn_once(width, height) {
                Ok(server) => return Ok(server),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow!("could not spawn a virtual display")))
    }

    fn spawn_once(width: u16, height: u16) -> Result<Xvfb> {
        let binary = std::env::var("XCOM_XVFB").unwrap_or_else(|_| "Xvfb".into());
        let log = std::env::temp_dir().join(format!("xcom-xvfb-{}.log", std::process::id()));
        let log_file =
            std::fs::File::create(&log).with_context(|| format!("creating {}", log.display()))?;
        let auth = write_auth_file()?;

        // Pipe for the display number: the server writes it once it is ready.
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            bail!("pipe() failed");
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);

        let mut cmd = Command::new(&binary);
        cmd.arg("-displayfd")
            .arg(write_fd.to_string())
            .arg("-auth")
            .arg(&auth)
            .args([
                "-screen",
                "0",
                &format!("{width}x{height}x24"),
                "-nolisten",
                "tcp",
                "+extension",
                "MIT-SHM",
                "+extension",
                "Composite",
                "+extension",
                "DAMAGE",
                "+extension",
                "XFIXES",
                "+extension",
                "XTEST",
            ])
            .stdout(Stdio::from(log_file.try_clone()?))
            .stderr(Stdio::from(log_file));
        #[cfg(target_os = "linux")]
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
        // Timestamp before the child exists: anything older than this belongs
        // to a previous server. (50 ms covers filesystem timestamp rounding.)
        let spawned_after = std::time::SystemTime::now() - Duration::from_millis(50);
        let mut child = cmd.spawn().with_context(|| {
            format!("spawning {binary} (install xorg-server-xvfb or set XCOM_XVFB)")
        })?;

        // The write end only matters in the child.
        unsafe { libc::close(write_fd) };

        // Non-blocking read with a deadline: a hung child holding the write end
        // must not hang daemon startup (the old `read_to_string` could).
        let flags = unsafe { libc::fcntl(read_fd, libc::F_GETFL) };
        if flags >= 0 {
            unsafe { libc::fcntl(read_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        }
        let file = unsafe { std::fs::File::from_raw_fd(read_fd) };
        let (num, displayfd) = read_display_number(file, &mut child, &binary, &log)?;

        let display = format!(":{num}");
        // Xvfb, the app and our own connection all use the cookie file.
        std::env::set_var("XAUTHORITY", &auth);
        // Wait until it actually accepts connections. A socket that predates
        // the spawn belongs to another (possibly dying) server: a connect can
        // succeed against it, so ownership is checked by the socket's mtime.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(Some(status)) = child.try_wait() {
                bail!(
                    "{binary} exited with {status} during startup; log: {}",
                    log.display()
                );
            }
            if x11rb::connect(Some(display.as_str())).is_ok() {
                if socket_is_fresh(&display, spawned_after) {
                    // Bare X servers have no XSETTINGS manager, so GTK falls
                    // back to the core X cursor. libXcursor reads these
                    // resources for every client; set them before apps start.
                    if let Ok((conn, screen)) = x11rb::connect(Some(display.as_str())) {
                        let root = conn.setup().roots[screen].root;
                        let res = b"Xcursor.theme: Adwaita\nXcursor.size: 24\n";
                        let _ = conn.change_property(
                            PropMode::REPLACE,
                            root,
                            AtomEnum::RESOURCE_MANAGER,
                            AtomEnum::STRING,
                            8,
                            res.len() as u32,
                            res,
                        );
                        let _ = conn.flush();
                    }
                    return Ok(Xvfb {
                        display,
                        log,
                        auth,
                        child,
                        _displayfd: displayfd,
                    });
                }
                terminate(&mut child);
                bail!("display {display} is served by another process (stale socket)");
            }
            if Instant::now() > deadline {
                terminate(&mut child);
                bail!("{binary} did not come up on {display}; log: {}", log.display());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        terminate(&mut self.child);
        let _ = std::fs::remove_file(&self.auth);
    }
}

/// Ask nicely first: Xvfb removes its lock file and unix socket on SIGTERM.
/// SIGKILL (the old behaviour) left both behind and poisoned later displays.
fn terminate(child: &mut Child) {
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// True when the display's unix socket was created after `after`.
///
/// Xvfb selects a display by its lock file, not by the socket path, so a
/// leftover socket can make it pick a number it cannot bind. A connect may
/// still succeed against the old server for a moment; the socket mtime tells
/// us whether the server we reached is the child we just started.
fn socket_is_fresh(display: &str, after: std::time::SystemTime) -> bool {
    let path = format!(
        "/tmp/.X11-unix/X{}",
        display.trim_start_matches(':')
    );
    match std::fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(mtime) => mtime >= after,
        // No socket path visible: trust the successful connect.
        Err(_) => true,
    }
}

/// Remove unix sockets for displays that have no lock file and no listener.
///
/// Only sockets owned by this user are considered. A live server always has a
/// lock file; a missing lock plus a refused connection is a crash leftover.
fn remove_stale_sockets() {
    use std::os::unix::fs::MetadataExt;

    let dir = std::path::Path::new("/tmp/.X11-unix");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let uid = unsafe { libc::getuid() };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(num) = name.to_str().and_then(|s| s.strip_prefix('X')) else {
            continue;
        };
        if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.uid() != uid {
            continue;
        }
        if std::path::Path::new(&format!("/tmp/.X{num}-lock")).exists() {
            continue;
        }
        if x11rb::connect(Some(&format!(":{num}"))).is_err() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn read_display_number(
    mut file: std::fs::File,
    child: &mut Child,
    binary: &str,
    log: &Path,
) -> Result<(String, std::fs::File)> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut buf = Vec::new();
    let mut tmp = [0u8; 64];
    loop {
        if let Some(status) = child.try_wait()? {
            bail!(
                "{binary} exited with {status} during startup; log: {}",
                log.display()
            );
        }
        if Instant::now() >= deadline {
            terminate(child);
            bail!(
                "{binary} did not report a display number; log: {}",
                log.display()
            );
        }
        match file.read(&mut tmp) {
            Ok(0) => std::thread::sleep(Duration::from_millis(20)),
            Ok(n) => {
                buf.extend_from_slice(&tmp[..n]);
                if buf.iter().any(u8::is_ascii_digit) {
                    let num: String = buf
                        .iter()
                        .filter(|b| b.is_ascii_digit())
                        .map(|b| *b as char)
                        .collect();
                    return Ok((num, file));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                terminate(child);
                return Err(anyhow!("reading {binary} display pipe failed: {e}"));
            }
        }
    }
}

/// Write a one-entry Xauthority file with a random MIT-MAGIC-COOKIE-1.
fn write_auth_file() -> Result<PathBuf> {
    let mut cookie = [0u8; 16];
    getrandom::fill(&mut cookie).map_err(|e| anyhow!("no OS entropy for X auth cookie: {e}"))?;
    let tag: Vec<u8> = cookie[..8].iter().flat_map(|b| format!("{b:02x}").into_bytes()).collect();
    let path = std::env::temp_dir().join(format!(
        "xcom-xauth-{}-{}",
        std::process::id(),
        String::from_utf8_lossy(&tag)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;

    // Xauthority entry: family (FamilyWild), empty address/number, name, data.
    const NAME: &[u8] = b"MIT-MAGIC-COOKIE-1";
    let mut entry = Vec::with_capacity(2 + 2 + 2 + 2 + NAME.len() + 2 + 16);
    entry.extend_from_slice(&u16::MAX.to_be_bytes()); // FamilyWild
    entry.extend_from_slice(&0u16.to_be_bytes()); // address length
    entry.extend_from_slice(&0u16.to_be_bytes()); // number length
    entry.extend_from_slice(&(NAME.len() as u16).to_be_bytes());
    entry.extend_from_slice(NAME);
    entry.extend_from_slice(&16u16.to_be_bytes());
    entry.extend_from_slice(&cookie);
    file.write_all(&entry)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Convenience: spawn a display and connect to it.
pub fn spawn_with_connection(
    width: u16,
    height: u16,
) -> Result<(Xvfb, x11rb::rust_connection::RustConnection, usize)> {
    let server = Xvfb::spawn(width, height)?;
    let (conn, screen) = x11rb::connect(Some(server.display.as_str()))
        .map_err(|e| anyhow!("connect to {}: {e}", server.display))?;
    Ok((server, conn, screen))
}
