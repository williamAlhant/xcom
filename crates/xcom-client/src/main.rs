//! xcom client: connect to a remote session and render its X windows as
//! native windows.
//!
//! The ssh bootstrap only exists to authenticate: `ssh user@host
//! 'xcomd serve --json'` returns one handshake line (channel address, token,
//! pinned certificate fingerprint), after which all traffic flows over the
//! dedicated TLS channel opened here.

mod clipboard;
mod hud;
mod keys;
mod link;
mod metrics;
mod ui;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use winit::event_loop::EventLoop;

use xcom_proto::{logln, Message, MSG_DAMAGE};
use xcom_transport::Channel;
use ui::{App, UserEvent};

#[derive(Parser)]
#[command(name = "xcom", about = "xcom client", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

// Parsed exactly once in main; the size difference between the connect flags
// and the tiny list commands does not matter.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Cmd {
    /// Connect to a remote session (SSH bootstrap or direct).
    Connect(ConnectArgs),
    /// List the sessions running on the remote host.
    Sessions(SessionsArgs),
    /// End a session on the remote host.
    Kill(KillArgs),
}

/// Which windowing backend to use.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum Backend {
    /// Prefer the X11/XWayland backend when a display is available.
    Auto,
    /// Force X11/XWayland.
    X11,
    /// Leave the environment alone (winit prefers Wayland when available).
    Wayland,
}

/// Internal marker set right before a failed X11 start re-execs this process on
/// Wayland. winit allows a single event loop per process and does not clear its
/// "already created" flag when initialization fails, so the retry must happen in
/// a fresh process.
const BACKEND_RESTART_ENV: &str = "XCOM_BACKEND_RESTART";

#[derive(clap::Args)]
struct ConnectArgs {
    /// ssh target (user@host): runs `xcomd serve --json` over ssh.
    #[arg(long)]
    ssh: Option<String>,
    /// Extra ssh arguments (e.g. "-p 2222 -i ~/.ssh/key").
    #[arg(long, allow_hyphen_values = true)]
    ssh_args: Option<String>,
    /// Session name on the remote host: attach to it, or create it.
    #[arg(long, default_value = "default")]
    session: String,
    /// Tear an existing session down and start a fresh one (destructive).
    #[arg(long)]
    replace: bool,
    /// End the session after N seconds without an attached client (0 = never).
    /// Only used when the session is created; ignored on reattach.
    #[arg(long, default_value_t = 0)]
    idle_timeout: u64,
    /// Direct address of a running daemon (skips the ssh bootstrap).
    #[arg(long)]
    direct: Option<String>,
    /// Control-channel token, hex (with --direct; visible in `ps`).
    #[arg(long)]
    token_control: Option<String>,
    /// Bulk-channel token, hex (with --direct; visible in `ps`).
    #[arg(long)]
    token_bulk: Option<String>,
    /// Certificate fingerprint, hex (with --direct).
    #[arg(long)]
    fingerprint: Option<String>,
    /// Override the address to dial (NAT / containers).
    #[arg(long)]
    connect: Option<String>,
    /// Exit after N seconds (0 = until the windows are closed).
    #[arg(long, default_value_t = 0)]
    seconds: u64,
    /// Command to launch in the remote session (e.g. "xterm").
    #[arg(long, allow_hyphen_values = true)]
    exec: Option<String>,
    /// Session display size to request when bootstrapping over ssh, as WxH
    /// (default: the largest monitor attached to this client). The session
    /// cannot grow later, and windows are capped at it.
    #[arg(long, value_parser = parse_spawn)]
    spawn: Option<(u16, u16)>,
    /// Write the largest window's final frame to this PPM file (test aid).
    #[arg(long)]
    dump_ppm: Option<std::path::PathBuf>,
    /// Map windows fullscreen (test aid for screen-vs-store comparison).
    #[arg(long)]
    fullscreen: bool,
    /// Enable local test aids (F12 store dump). Off by default so F12 reaches
    /// the remote application.
    #[arg(long)]
    debug_tools: bool,
    /// Show the metrics HUD (toggle in-session with Ctrl+Shift+M).
    #[arg(long)]
    hud: bool,
    /// Do not mirror the clipboard. Mirroring means the remote can read your
    /// local selection, so it can be switched off.
    #[arg(long)]
    no_clipboard_sync: bool,
    /// Windowing backend (Linux only). `auto` prefers X11/XWayland because
    /// per-window placement and clipboard mirroring depend on it.
    #[arg(long, value_enum, default_value = "auto")]
    backend: Backend,
    /// What closing the last window does: `ask` shows an in-window dialog,
    /// `close` closes it (the session ends), `disconnect` leaves the session
    /// running. Scripts and tests use `close`/`disconnect`.
    #[arg(long, value_enum, default_value = "ask")]
    close_action: ui::CloseAction,
    /// Do not reconnect automatically after a dropped --ssh link.
    #[arg(long)]
    no_reconnect: bool,
    /// Test aid: ask the session to drop the link after N seconds.
    #[arg(long, default_value_t = 0)]
    drop_after: u64,
    /// Test aid: type this text (Latin-1) after the session comes up.
    #[arg(long, allow_hyphen_values = true)]
    type_text: Option<String>,
    /// Seconds to wait before typing (lets windows open and take focus).
    #[arg(long, default_value_t = 3)]
    type_delay: u64,
    /// Test aid: resize the window to WxH after --resize-after seconds.
    #[arg(long)]
    resize_to: Option<String>,
    #[arg(long, default_value_t = 2)]
    resize_after: u64,
    /// Test aid: dump the store N seconds after start (mid-session).
    #[arg(long, default_value_t = 0)]
    dump_after: u64,
    /// Test aid: close one window N seconds after start (session must survive).
    #[arg(long, default_value_t = 0)]
    close_after: u64,
}

#[derive(clap::Args)]
struct SessionsArgs {
    /// ssh target (user@host).
    #[arg(long)]
    ssh: String,
    /// Extra ssh arguments (e.g. "-p 2222 -i ~/.ssh/key").
    #[arg(long, allow_hyphen_values = true)]
    ssh_args: Option<String>,
}

#[derive(clap::Args)]
struct KillArgs {
    /// ssh target (user@host).
    #[arg(long)]
    ssh: String,
    /// Extra ssh arguments (e.g. "-p 2222 -i ~/.ssh/key").
    #[arg(long, allow_hyphen_values = true)]
    ssh_args: Option<String>,
    /// Session name.
    #[arg(long, default_value = "default")]
    session: String,
}

struct Handshake {
    addr: String,
    control_token: [u8; 32],
    bulk_token: [u8; 32],
    fingerprint: String,
    ssh: Option<Child>,
}

fn json_field(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Choose the windowing backend and apply it to the environment.
///
/// The X11/XWayland backend is preferred because Wayland has no global window
/// positions (`xdg_popup` is not available in winit 0.30) and its clipboard can
/// only be observed through XWayland. `--backend wayland` keeps native Wayland.
///
/// Returns the chosen backend and, when `WAYLAND_DISPLAY` was hidden, its old
/// value so a failed X11 start can fall back.
fn apply_backend(choice: Backend) -> Result<(Backend, Option<std::ffi::OsString>)> {
    if !cfg!(target_os = "linux") {
        // winit uses the native backend here (Win32, Cocoa). `DISPLAY` may be
        // set by an unrelated X server or X forwarding, but it means nothing
        // to winit on this platform.
        if choice != Backend::Auto {
            logln!("[xcom] --backend is Linux-only; using the native backend");
        }
        return Ok((Backend::Auto, None));
    }
    // A failed X11 start re-execs this process with this marker set; the parent
    // already restored WAYLAND_DISPLAY. Consume it so a child of the child (or a
    // later run in the same environment) is not affected. An explicit
    // `--backend x11` is overridden: the whole point of the restart is Wayland.
    let restarted = std::env::var_os(BACKEND_RESTART_ENV).is_some();
    if restarted {
        std::env::remove_var(BACKEND_RESTART_ENV);
    }
    let wayland = std::env::var_os("WAYLAND_DISPLAY");
    let x11 = std::env::var_os("DISPLAY").is_some();
    let chosen = if restarted {
        Backend::Wayland
    } else {
        match choice {
            Backend::X11 => {
                if !x11 {
                    bail!("--backend x11 needs DISPLAY (is XWayland running?)");
                }
                Backend::X11
            }
            Backend::Wayland => Backend::Wayland,
            Backend::Auto => {
                if x11 {
                    Backend::X11
                } else {
                    Backend::Wayland
                }
            }
        }
    };
    let mut saved = None;
    if chosen == Backend::X11 {
        if let Some(w) = wayland {
            // Called before the event loop or any threads exist.
            std::env::remove_var("WAYLAND_DISPLAY");
            saved = Some(w);
        }
        logln!(
            "[xcom] backend: X11/XWayland (popup placement and clipboard mirroring need it; \
             --backend wayland opts out)"
        );
    } else {
        logln!(
            "[xcom] backend: Wayland (popups are compositor-placed; local clipboard changes are \
             only visible through XWayland while it is focused)"
        );
    }
    Ok((chosen, saved))
}

/// Numeric JSON field (`"proto":6`), unlike `json_field` which expects a
/// quoted string value.
fn json_number(line: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{key}\":");
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn token32(bytes: Vec<u8>) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("token must be exactly 32 bytes"))
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        bail!("odd-length hex string");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).context("bad hex"))
        .collect()
}

/// Single-quote a string for the remote POSIX shell.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Parse a `WxH` display size (clap value parser for `--spawn`).
fn parse_spawn(s: &str) -> Result<(u16, u16), String> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| format!("expected WxH, got {s:?}"))?;
    let w: u16 = w.parse().map_err(|_| format!("bad width in {s:?}"))?;
    let h: u16 = h.parse().map_err(|_| format!("bad height in {s:?}"))?;
    if w == 0 || h == 0 {
        return Err(format!("width and height must be non-zero, got {s:?}"));
    }
    Ok((w, h))
}

/// Build the event loop (with the X11 -> Wayland fallback used by `--backend`).
///
/// The fallback cannot rebuild the event loop in this process: winit allows one
/// event loop per process and leaves its "already created" flag set even when
/// initialization fails. Instead the process re-execs itself with Wayland
/// forced, which is invisible because this runs before the session starts.
fn create_event_loop(
    backend: Backend,
    saved_wayland: Option<std::ffi::OsString>,
) -> Result<EventLoop<UserEvent>> {
    match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(el) => Ok(el),
        Err(e) => {
            // We may have hidden a perfectly good Wayland session behind a
            // stale DISPLAY; retry there before giving up.
            if backend == Backend::X11 {
                if let Some(wayland) = saved_wayland {
                    logln!("[xcom] X11 backend unavailable ({e}); retrying on Wayland");
                    let restart = restart_on_wayland(wayland);
                    return Err(e).with_context(|| {
                        format!(
                            "creating the event loop (a display server is required for the \
                             client); restarting on Wayland also failed: {restart}{}",
                            static_build_hint()
                        )
                    });
                }
            }
            Err(e).with_context(|| {
                format!(
                    "creating the event loop (a display server is required for the client){}",
                    static_build_hint()
                )
            })
        }
    }
}

/// Extra explanation for the one build that can never load a windowing library.
fn static_build_hint() -> &'static str {
    // winit and softbuffer load libX11/libwayland with dlopen. A fully static
    // musl binary has no dynamic loader, so both backends are unusable.
    if cfg!(all(
        target_os = "linux",
        target_env = "musl",
        target_feature = "crt-static"
    )) {
        "; this xcom build is statically linked, and winit loads libX11/libwayland \
         with dlopen, which static musl does not support: use the dynamically linked \
         Linux client"
    } else {
        ""
    }
}

/// Replace this process with an identical command forced onto Wayland.
///
/// Called before any threads, windows or ssh child exist, so `exec` is safe.
/// Returns only if the restart failed (on success this function never returns).
#[cfg(unix)]
fn restart_on_wayland(wayland: std::ffi::OsString) -> std::io::Error {
    use std::os::unix::process::CommandExt as _;

    // Put back what apply_backend() hid and tell the new process to skip X11.
    std::env::set_var("WAYLAND_DISPLAY", wayland);
    std::env::set_var(BACKEND_RESTART_ENV, "wayland");
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return e,
    };
    Command::new(exe).args(std::env::args_os().skip(1)).exec()
}

#[cfg(not(unix))]
fn restart_on_wayland(_wayland: std::ffi::OsString) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "restarting on Wayland is only supported on Unix",
    )
}

/// Size of the local desktop in physical pixels.
///
/// Queried from the OS rather than winit: monitors are only available from
/// `ActiveEventLoop`, which is too late (the session is bootstrapped before the
/// loop runs). `None` disables the size request, leaving the daemon default.
#[cfg(target_os = "linux")]
fn display_size() -> Option<(u16, u16)> {
    use x11rb::connection::Connection as _;
    // The X root window is the whole desktop (the union of monitors), and on
    // Wayland this goes through XWayland when it is there.
    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = &conn.setup().roots[screen];
    Some((root.width_in_pixels, root.height_in_pixels))
}

#[cfg(target_os = "windows")]
fn display_size() -> Option<(u16, u16)> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    };
    // Physical pixels for a DPI-aware process: winit sets per-monitor v2 when
    // the event loop is created (which runs before this).
    let (w, h) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    if w <= 0 || h <= 0 {
        return None;
    }
    Some((
        (w as u32).min(u16::MAX as u32) as u16,
        (h as u32).min(u16::MAX as u32) as u16,
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn display_size() -> Option<(u16, u16)> {
    None
}

/// Run `xcomd attach --create --json` over ssh and parse the handshake line.
#[allow(clippy::too_many_arguments)] // bootstrap options, one per caller flag
fn bootstrap_ssh(
    target: &str,
    ssh_args: Option<&str>,
    exec: Option<&str>,
    spawn: Option<(u16, u16)>,
    create: bool,
    drop_after: u64,
    session: &str,
    replace: bool,
    idle_timeout: u64,
) -> Result<Handshake> {
    // Let XCOM_DEBUG follow the session so the daemon traces too.
    let dbg = if xcom_proto::debug() {
        "XCOM_DEBUG=1 "
    } else {
        ""
    };
    let remote = {
        // attach-or-create: on a reattach `--exec`/`--spawn` are ignored by the
        // owner (the app is already running); `--replace` is the way to change
        // them. A reconnect is attach-only, so a dead session is an error
        // instead of silently starting a new app.
        let mut cmd = String::from("xcomd attach --json");
        if create {
            cmd.push_str(" --create");
        }
        cmd.push_str(&format!(" --name {}", shell_quote(session)));
        if create {
            if replace {
                cmd.push_str(" --replace");
            }
            if idle_timeout > 0 {
                cmd.push_str(&format!(" --idle-timeout {idle_timeout}"));
            }
            if drop_after > 0 {
                cmd.push_str(&format!(" --drop-after {drop_after}"));
            }
            if let Some((w, h)) = spawn {
                logln!("[xcom] requesting a {w}x{h} session display");
                cmd.push_str(&format!(" --spawn {w}x{h}"));
            }
            if let Some(exec) = exec {
                cmd.push_str(&format!(" --exec {}", shell_quote(exec)));
            }
        }
        format!("{dbg}{cmd}")
    };
    let mut cmd = Command::new("ssh");
    cmd.arg("-o").arg("BatchMode=yes");
    if let Some(extra) = ssh_args {
        for a in extra.split_whitespace() {
            cmd.arg(a);
        }
    }
    cmd.arg(target).arg(remote);
    cmd.stderr(Stdio::inherit());
    let mut child = cmd
        .stdout(Stdio::piped())
        .spawn()
        .context("spawning ssh (is the OpenSSH client installed?)")?;
    let stdout = child.stdout.take().context("ssh stdout")?;
    let mut lines = BufReader::new(stdout).lines();
    loop {
        let line = lines
            .next()
            .context("the ssh session ended before the daemon sent its handshake line")?
            .context("reading handshake line")?;
        if line.starts_with('{') {
            let addr = json_field(&line, "connect").context("handshake line: connect")?;
            let control = hex_decode(
                &json_field(&line, "token_control").context("handshake line: token_control")?,
            )?;
            let bulk = hex_decode(
                &json_field(&line, "token_bulk").context("handshake line: token_bulk")?,
            )?;
            let fingerprint =
                json_field(&line, "cert_sha256").context("handshake line: cert_sha256")?;
            // A daemon from another revision can look like a bug (e.g. a
            // missing feature that exists in this client).
            match json_field(&line, "build") {
                Some(remote) if remote != xcom_proto::BUILD => logln!(
                    "[xcom] warning: remote xcomd build {remote} differs from this client \
                     ({}); update the remote binary",
                    xcom_proto::BUILD
                ),
                Some(_) => {}
                None => logln!(
                    "[xcom] warning: remote xcomd does not report its build (older version); \
                     update it if something misbehaves"
                ),
            }
            // The JSON is informational, but a version mismatch here means the
            // remote binary is from a different build: say so plainly instead of
            // failing later on an unknown message type. (The TLS hello checks it
            // too; this is the friendlier early error.)
            match json_number(&line, "proto") {
                Some(p) if p == xcom_proto::PROTO_VERSION as u64 => {}
                Some(p) => bail!(
                    "remote speaks protocol {p}, this client speaks {} (update the other side)",
                    xcom_proto::PROTO_VERSION
                ),
                None => bail!("handshake line: missing proto"),
            }
            // Reattaching does not relaunch the app, so say which one is running:
            // a stale session is the surprising case.
            if let Some(app) = json_field(&line, "app") {
                if !app.is_empty() {
                    logln!("[xcom] session app: {app}");
                }
            }
            return Ok(Handshake {
                addr,
                control_token: token32(control)?,
                bulk_token: token32(bulk)?,
                fingerprint,
                ssh: Some(child),
            });
        }
        // Anything else is daemon log output; show it.
        logln!("[remote] {line}");
    }
}

fn bootstrap_direct(
    addr: &str,
    control: &str,
    bulk: &str,
    fingerprint: &str,
) -> Result<Handshake> {
    Ok(Handshake {
        addr: addr.to_string(),
        control_token: token32(hex_decode(control)?)?,
        bulk_token: token32(hex_decode(bulk)?)?,
        fingerprint: fingerprint.to_string(),
        ssh: None,
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Connect(args) => connect(args),
        Cmd::Sessions(args) => remote_command("xcomd sessions", &args.ssh, args.ssh_args.as_deref()),
        Cmd::Kill(args) => remote_command(
            &format!("xcomd kill --session {}", shell_quote(&args.session)),
            &args.ssh,
            args.ssh_args.as_deref(),
        ),
    }
}

/// Run a one-shot xcomd command over ssh (`sessions`, `kill`).
fn remote_command(remote: &str, target: &str, ssh_args: Option<&str>) -> Result<()> {
    let mut cmd = Command::new("ssh");
    cmd.arg("-o").arg("BatchMode=yes");
    if let Some(extra) = ssh_args {
        for a in extra.split_whitespace() {
            cmd.arg(a);
        }
    }
    cmd.arg(target).arg(remote);
    let status = cmd
        .status()
        .context("spawning ssh (is the OpenSSH client installed?)")?;
    if !status.success() {
        bail!("remote command failed ({status})");
    }
    Ok(())
}

fn connect(args: ConnectArgs) -> Result<()> {
    // Must run before the event loop (and before any thread) reads the
    // environment.
    let (backend, saved_wayland) = apply_backend(args.backend)?;

    // Monitors must be known before the bootstrap: the session display is
    // sized from them and Xvfb cannot grow afterwards.
    let event_loop = create_event_loop(backend, saved_wayland)?;
    let monitor = display_size();

    let mut handshake = match (&args.ssh, &args.direct) {
        (Some(target), _) => bootstrap_ssh(
            target,
            args.ssh_args.as_deref(),
            args.exec.as_deref(),
            args.spawn.or(monitor),
            true,
            args.drop_after,
            &args.session,
            args.replace,
            args.idle_timeout,
        )?,
        (None, Some(addr)) => {
            if args.spawn.is_some() {
                logln!(
                    "[xcom] note: --spawn only applies to the ssh bootstrap; the direct daemon is already running"
                );
            }
            if args.session != "default" || args.replace || args.idle_timeout > 0 {
                logln!(
                    "[xcom] note: --session/--replace/--idle-timeout only apply to the ssh bootstrap; the direct daemon is already running"
                );
            }
            if args.token_control.is_some() || args.token_bulk.is_some() {
                logln!(
                    "[xcom] warning: tokens on the command line are visible to local users (ps); use --ssh"
                );
            }
            bootstrap_direct(
                addr,
                args.token_control
                    .as_deref()
                    .context("--direct needs --token-control")?,
                args.token_bulk
                    .as_deref()
                    .context("--direct needs --token-bulk")?,
                args.fingerprint
                    .as_deref()
                    .context("--direct needs --fingerprint")?,
            )?
        }
        (None, None) => bail!("either --ssh user@host or --direct addr is required"),
    };
    if let Some(override_addr) = &args.connect {
        handshake.addr = override_addr.clone();
    }

    logln!(
        "[xcom] {} connecting to {} (fingerprint {}…)",
        xcom_proto::BUILD,
        handshake.addr,
        &handshake.fingerprint[..16.min(handshake.fingerprint.len())]
    );

    let proxy = event_loop.create_proxy();
    // Every message carries its enqueue time: the writer measures how long it
    // waited before reaching the socket (the HUD's queue_ms).
    let queue_us = Arc::new(AtomicU32::new(0));

    // One stable input handle for the UI, the clipboard worker and the test
    // aids. The supervisor swaps the underlying sender on reconnect; messages
    // sent while disconnected go to the dead channel and are dropped.
    let (initial_tx, _initial_rx) = mpsc::channel::<(Message, Instant)>();
    let sink = link::InputSink::new(initial_tx);

    let endpoint = link::Endpoint {
        addr: handshake.addr.clone(),
        control_token: handshake.control_token,
        bulk_token: handshake.bulk_token,
        fingerprint: handshake.fingerprint.clone(),
    };
    let current = link::start(&endpoint, &sink, &proxy, Arc::clone(&queue_us))?;

    // Seed the local clipboard from the remote's current selection; after this
    // the daemon pushes changes and the watcher mirrors local ones. On every
    // reattach the server resyncs the client, and this is repeated after the
    // reconnected event below.
    if !args.no_clipboard_sync {
        sink.send(Message::ClipboardGet {
            target: xcom_proto::CLIPBOARD_CLIPBOARD,
        });
    }

    if let Some(text) = args.type_text.clone() {
        let sink = sink.clone();
        let delay = args.type_delay;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(delay));
            logln!("[xcom] typing {text:?} t_ns={}", xcom_proto::now_ns());
            for c in text.chars() {
                // Control characters map to their X keysyms, not their codes.
                let sym = match c {
                    '\u{8}' => 0xff08, // BackSpace
                    '\n' => 0xff0d,    // Return
                    '\t' => 0xff09,    // Tab
                    _ => {
                        let cp = c as u32;
                        // Match the client's keysym convention above Latin-1.
                        if cp >= 0x100 {
                            0x0100_0000 + cp
                        } else {
                            cp
                        }
                    }
                };
                for pressed in [true, false] {
                    sink.send_at(Message::InputKey { sym, pressed }, Instant::now());
                }
                std::thread::sleep(Duration::from_millis(30));
            }
        });
    }

    if let Some(spec) = args.resize_to.clone() {
        if let Some((w, h)) = spec
            .split_once('x')
            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        {
            let proxy_timer = proxy.clone();
            let after = args.resize_after;
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(after));
                proxy_timer.send_event(UserEvent::ResizeLocal { w, h }).ok();
            });
        }
    }

    if args.close_after > 0 {
        let proxy_timer = proxy.clone();
        let after = args.close_after;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(after));
            proxy_timer.send_event(UserEvent::CloseOne).ok();
        });
    }

    if args.dump_after > 0 {
        let proxy_timer = proxy.clone();
        let after = args.dump_after;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(after));
            proxy_timer.send_event(UserEvent::Dump).ok();
        });
    }

    if args.seconds > 0 {
        let proxy_timer = proxy.clone();
        let secs = args.seconds;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            proxy_timer.send_event(UserEvent::Quit).ok();
        });
    }

    // A dropped --ssh link is re-established in the background. --direct has
    // no way to re-authenticate (single-use tokens arrive over ssh) and keeps
    // the old exit-on-loss behaviour.
    let reconnect = args.ssh.is_some() && !args.no_reconnect;
    if reconnect {
        let sup = SupervisorArgs {
            ssh: args.ssh.clone().unwrap_or_default(),
            ssh_args: args.ssh_args.clone(),
            exec: args.exec.clone(),
            spawn: args.spawn.or(monitor),
            session: args.session.clone(),
            connect: args.connect.clone(),
            clipboard: !args.no_clipboard_sync,
        };
        let sup_sink = sink.clone();
        let sup_proxy = proxy.clone();
        let sup_queue = Arc::clone(&queue_us);
        std::thread::spawn(move || supervise(current, sup, sup_sink, sup_proxy, sup_queue));
    } else {
        let loss_proxy = proxy.clone();
        std::thread::spawn(move || {
            current.wait_down();
            current.stop();
            loss_proxy
                .send_event(UserEvent::ConnectionLost { reconnect: false })
                .ok();
        });
    }

    let mut app = App::new(
        proxy,
        sink,
        ui::AppOptions {
            dump: args.dump_ppm.clone(),
            fullscreen: args.fullscreen,
            debug_tools: args.debug_tools,
            hud: args.hud,
            queue_us,
            clipboard_sync: !args.no_clipboard_sync,
            close_action: args.close_action,
            monitor,
        },
    );
    let result = event_loop.run_app(&mut app).context("event loop");

    if let Some(mut ssh) = handshake.ssh.take() {
        let _ = ssh.kill();
        let _ = ssh.wait();
    }
    result
}

/// What the reconnect supervisor needs to bootstrap a fresh attachment.
struct SupervisorArgs {
    ssh: String,
    ssh_args: Option<String>,
    exec: Option<String>,
    spawn: Option<(u16, u16)>,
    session: String,
    connect: Option<String>,
    clipboard: bool,
}

/// Reconnect backoff: 1, 2, 4, 8, then 15 s forever.
const BACKOFF: [u64; 5] = [1, 2, 4, 8, 15];

/// Errors that mean retrying cannot help: the session is gone, busy, or the
/// bootstrap is incompatible. Network/ssh failures are retried.
fn permanent_failure(error: &str) -> bool {
    error.contains("is gone")
        || error.contains("is not running")
        || error.contains("session busy")
        || error.contains("did not return a handshake")
}

/// Keep a --ssh session attached: on link loss, re-bootstrap attach-only and
/// swap in the new link. Runs on its own thread; only the proxy talks to the UI.
fn supervise(
    mut link: link::Link,
    args: SupervisorArgs,
    sink: link::InputSink,
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    queue_us: Arc<AtomicU32>,
) {
    loop {
        link.wait_down();
        link.stop();
        proxy
            .send_event(UserEvent::ConnectionLost { reconnect: true })
            .ok();
        let mut attempt = 0usize;
        loop {
            let delay = BACKOFF[attempt.min(BACKOFF.len() - 1)];
            attempt += 1;
            proxy
                .send_event(UserEvent::Reconnecting {
                    attempt: attempt as u32,
                })
                .ok();
            std::thread::sleep(Duration::from_secs(delay));
            let handshake = match bootstrap_ssh(
                &args.ssh,
                args.ssh_args.as_deref(),
                args.exec.as_deref(),
                args.spawn,
                false,
                0,
                &args.session,
                false,
                0,
            ) {
                Ok(handshake) => handshake,
                Err(e) => {
                    let msg = format!("{e:#}");
                    if permanent_failure(&msg) {
                        proxy.send_event(UserEvent::SessionGone(msg)).ok();
                        return;
                    }
                    proxy.send_event(UserEvent::ReconnectFailed(msg)).ok();
                    continue;
                }
            };
            if let Some(mut ssh) = handshake.ssh {
                // `attach` exits after printing the handshake.
                let _ = ssh.wait();
            }
            let mut endpoint = link::Endpoint {
                addr: handshake.addr,
                control_token: handshake.control_token,
                bulk_token: handshake.bulk_token,
                fingerprint: handshake.fingerprint,
            };
            if let Some(connect) = &args.connect {
                endpoint.addr = connect.clone();
            }
            match link::start(&endpoint, &sink, &proxy, Arc::clone(&queue_us)) {
                Ok(next) => {
                    logln!("[xcom] reconnected (attempt {attempt})");
                    if args.clipboard {
                        sink.send(Message::ClipboardGet {
                            target: xcom_proto::CLIPBOARD_CLIPBOARD,
                        });
                    }
                    proxy.send_event(UserEvent::Reconnected).ok();
                    link = next;
                    break;
                }
                Err(e) => {
                    proxy
                        .send_event(UserEvent::ReconnectFailed(format!("{e:#}")))
                        .ok();
                }
            }
        }
    }
}

/// Reader half: blocking, no polling. Dispatches window events to the UI and
/// measures RTT when a pong matches the outstanding ping.
pub(crate) fn control_reader_loop(
    mut reader: xcom_transport::ChannelReader,
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    pending: Arc<Mutex<Option<(u64, Instant)>>>,
    stop: Arc<AtomicBool>,
) {
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match reader.recv() {
            Ok(Some((ty, payload))) => {
                let Ok(msg) = Message::decode(ty, &payload) else {
                    logln!("[xcom] control: undecodable message type {ty}");
                    continue;
                };
                let event = match msg {
                    Message::WindowCreate {
                        id,
                        x,
                        y,
                        w,
                        h,
                        title,
                        kind,
                        parent,
                        override_redirect,
                    } => Some(UserEvent::Open {
                        id,
                        x,
                        y,
                        w,
                        h,
                        title,
                        kind,
                        parent,
                        override_redirect,
                    }),
                    Message::WindowGeometry { id, x, y, w, h } => {
                        Some(UserEvent::Geometry { id, x, y, w, h })
                    }
                    Message::ClipboardData { text, .. } => {
                        Some(UserEvent::ClipboardData { text })
                    }
                    Message::Cursor {
                        xhot,
                        yhot,
                        w,
                        h,
                        pixels,
                    } => Some(UserEvent::Cursor {
                        xhot,
                        yhot,
                        w,
                        h,
                        pixels,
                    }),
                    Message::Stats {
                        interval_ms,
                        frames,
                        rects,
                        raw_bytes,
                        comp_bytes,
                        capture_us,
                        encode_us,
                        windows,
                    } => Some(UserEvent::Stats(crate::metrics::ServerStats {
                        interval_ms,
                        frames,
                        rects,
                        raw_bytes,
                        comp_bytes,
                        capture_us,
                        encode_us,
                        windows,
                    })),
                    Message::Pong { t } => {
                        // `t` is echoed verbatim, so this works across hosts
                        // with unrelated clocks.
                        let mut p = pending.lock().unwrap();
                        if let Some((sent_t, at)) = *p {
                            if sent_t == t {
                                let ms = at.elapsed().as_millis().min(u32::MAX as u128) as u32;
                                proxy.send_event(UserEvent::Rtt { ms }).ok();
                                *p = None;
                            }
                        }
                        None
                    }
                    Message::WindowUnmapped { id } => Some(UserEvent::Unmap { id }),
                    Message::WindowMapped { id } => Some(UserEvent::Map { id }),
                    Message::WindowDestroy { id } => Some(UserEvent::Close { id }),
                    Message::Display { w, h } => Some(UserEvent::Display { w, h }),
                    Message::SyncDone => Some(UserEvent::SyncDone),
                    _ => None,
                };
                if let Some(ev) = event {
                    proxy.send_event(ev).ok();
                }
            }
            Ok(None) => return, // clean EOF
            Err(e) => {
                logln!("[xcom] control channel: {e}");
                return;
            }
        }
    }
}

/// Writer half: writes queued input immediately (no read timeout to wait for)
/// and sends the keepalive ping when idle.
/// Send one keepalive/RTT probe and remember when it was sent.
fn send_ping(
    writer: &mut xcom_transport::ChannelWriter,
    pending: &Mutex<Option<(u64, Instant)>>,
) -> bool {
    let t = xcom_proto::now_ns();
    let ping = Message::Ping { t };
    if writer.send(ping.msg_type(), &ping.encode()).is_err() {
        return false;
    }
    *pending.lock().unwrap() = Some((t, Instant::now()));
    true
}

pub(crate) fn control_writer_loop(
    mut writer: xcom_transport::ChannelWriter,
    input_rx: mpsc::Receiver<(Message, Instant)>,
    queue_us: Arc<AtomicU32>,
    pending: Arc<Mutex<Option<(u64, Instant)>>>,
    stop: Arc<AtomicBool>,
) {
    // First ping immediately, so the HUD has a round-trip time from the start.
    if !send_ping(&mut writer, &pending) {
        writer.force_close();
        return;
    }
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        // Liveness: a ping outstanding for 20 s means the peer is gone. This is
        // the app-level equivalent of the old "no traffic for 20 s" check.
        if let Some((_, at)) = *pending.lock().unwrap() {
            if at.elapsed() > Duration::from_secs(20) {
                logln!("[xcom] no pong for 20 s; assuming the peer is gone");
                writer.force_close();
                return;
            }
        }
        match input_rx.recv_timeout(Duration::from_secs(5)) {
            Ok((msg, at)) => {
                let waited = at.elapsed().as_micros().min(u32::MAX as u128) as u32;
                queue_us.fetch_max(waited, Ordering::Relaxed);
                let bye = msg == Message::Bye;
                if writer.send(msg.msg_type(), &msg.encode()).is_err() {
                    writer.force_close();
                    return;
                }
                if bye {
                    writer.shutdown();
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                if !send_ping(&mut writer, &pending) {
                    writer.force_close();
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

pub(crate) fn bulk_loop(
    mut bulk: Channel,
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
) -> Result<()> {
    loop {
        match bulk.recv() {
            Ok(Some((MSG_DAMAGE, payload))) => {
                let frame = xcom_proto::DamageFrame::decode(&payload)?;
                proxy.send_event(UserEvent::Frame(frame)).ok();
            }
            Ok(Some((ty, _))) => logln!("[xcom] bulk: unexpected msg type {ty}"),
            Ok(None) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_spawn;

    #[test]
    fn spawn_sizes_parse() {
        assert_eq!(parse_spawn("1920x1080"), Ok((1920, 1080)));
        assert!(parse_spawn("1920").is_err());
        assert!(parse_spawn("1920x").is_err());
        assert!(parse_spawn("0x1080").is_err());
        assert!(parse_spawn("axb").is_err());
    }
}
