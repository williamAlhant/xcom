//! Virtual display lifecycle: spawn and own an `Xvfb` instance.
//!
//! Xvfb is the one external dependency of the server half (see the M0 report
//! for the vendored-server upgrade path). Everything else is pure Rust.

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
    /// Spawn on the first free display number in 20..200.
    pub fn spawn(width: u16, height: u16) -> Result<Xvfb> {
        for n in 20..200 {
            let socket = format!("/tmp/.X11-unix/X{n}");
            let lock = format!("/tmp/.X{n}-lock");
            if !std::path::Path::new(&socket).exists() && !std::path::Path::new(&lock).exists() {
                return Xvfb::spawn_at(n, width, height);
            }
        }
        bail!("no free display number in 20..200")
    }

    pub fn spawn_at(num: u32, width: u16, height: u16) -> Result<Xvfb> {
        let binary = std::env::var("XCOM_XVFB").unwrap_or_else(|_| "Xvfb".into());
        let display = format!(":{num}");
        let log = std::env::temp_dir().join(format!("xcom-xvfb-{num}.log"));
        let log_file = std::fs::File::create(&log)
            .with_context(|| format!("creating log {}", log.display()))?;

        let mut child = Command::new(&binary)
            .args([
                display.as_str(),
                "-screen",
                &format!("0 {width}x{height}x24"),
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
            .with_context(|| format!("spawning {binary} (install xorg-server-xvfb or set XCOM_XVFB)"))?;

        // Wait for the server to accept connections.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait()? {
                bail!(
                    "{binary} exited with {status} during startup; log: {}",
                    log.display()
                );
            }
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
        // Best effort: clear the lock file if the server left one behind.
        let num = self.display.trim_start_matches(':');
        let _ = std::fs::remove_file(format!("/tmp/.X{num}-lock"));
        let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{num}"));
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
