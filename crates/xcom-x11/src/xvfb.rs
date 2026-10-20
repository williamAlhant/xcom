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

pub struct Xvfb {
    pub display: String,
    pub log: PathBuf,
    auth: PathBuf,
    child: Child,
}

impl Xvfb {
    /// Spawn on a free display number chosen by the X server itself.
    pub fn spawn(width: u16, height: u16) -> Result<Xvfb> {
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
        let mut file = unsafe { std::fs::File::from_raw_fd(read_fd) };
        let num = read_display_number(&mut child, &mut file, &binary, &log)?;

        let display = format!(":{num}");
        // Xvfb, the app and our own connection all use the cookie file.
        std::env::set_var("XAUTHORITY", &auth);
        // Wait until it actually accepts connections.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if x11rb::connect(Some(display.as_str())).is_ok() {
                return Ok(Xvfb {
                    display,
                    log,
                    auth,
                    child,
                });
            }
            if Instant::now() > deadline {
                let _ = child.kill();
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
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.auth);
    }
}

fn read_display_number(
    child: &mut Child,
    file: &mut std::fs::File,
    binary: &str,
    log: &Path,
) -> Result<String> {
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
            let _ = child.kill();
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
                    return Ok(num);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => {
                let _ = child.kill();
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
