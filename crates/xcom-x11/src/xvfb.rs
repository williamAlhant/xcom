//! Virtual display lifecycle: spawn and own an `Xvfb` instance.
//!
//! Xvfb is the one external dependency of the server half (see the M0 report
//! for the vendored-server upgrade path). Everything else is pure Rust.
//!
//! The display number is picked by Xvfb itself (`-displayfd`): scanning
//! /tmp/.X11-unix for free numbers races between concurrent sessions, and the
//! loser ends up attached to the winner's X server — which then dies with the
//! winner's daemon (seen as `shm_get_image: Broken pipe` mid-capture).

use std::io::Read;
use std::os::unix::io::FromRawFd;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

pub struct Xvfb {
    pub display: String,
    pub log: PathBuf,
    child: Child,
}

impl Xvfb {
    /// Spawn on a free display number chosen by the X server itself.
    pub fn spawn(width: u16, height: u16) -> Result<Xvfb> {
        let binary = std::env::var("XCOM_XVFB").unwrap_or_else(|_| "Xvfb".into());
        let log = std::env::temp_dir().join(format!("xcom-xvfb-{}.log", std::process::id()));
        let log_file =
            std::fs::File::create(&log).with_context(|| format!("creating {}", log.display()))?;

        // Pipe for the display number: the server writes it once it is ready.
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            bail!("pipe() failed");
        }
        let (read_fd, write_fd) = (fds[0], fds[1]);

        let mut child = Command::new(&binary)
            .arg("-displayfd")
            .arg(write_fd.to_string())
            .args([
                "-screen",
                "0",
                &format!("{width}x{height}x24"),
                "-ac",
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
            .stderr(Stdio::from(log_file))
            .spawn()
            .with_context(|| {
                format!("spawning {binary} (install xorg-server-xvfb or set XCOM_XVFB)")
            })?;

        // The write end only matters in the child.
        unsafe { libc::close(write_fd) };

        let mut file = unsafe { std::fs::File::from_raw_fd(read_fd) };
        let mut num = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait()? {
                bail!(
                    "{binary} exited with {status} during startup; log: {}",
                    log.display()
                );
            }
            match file.read_to_string(&mut num) {
                Ok(_) if !num.trim().is_empty() => break,
                Ok(_) | Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                _ => {
                    let _ = child.kill();
                    bail!(
                        "{binary} did not report a display number; log: {}",
                        log.display()
                    );
                }
            }
        }

        let display = format!(":{}", num.trim());
        // Wait until it actually accepts connections.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if x11rb::connect(Some(display.as_str())).is_ok() {
                return Ok(Xvfb {
                    display,
                    log,
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
    }
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
