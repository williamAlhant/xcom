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
mod metrics;
mod ui;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use winit::event_loop::EventLoop;

use xcom_proto::{logln, Message, MSG_DAMAGE};
use xcom_transport::{Channel, Role};
use ui::{App, UserEvent};

#[derive(Parser)]
#[command(name = "xcom", about = "xcom client", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Connect to a remote session (SSH bootstrap or direct).
    Connect(ConnectArgs),
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

#[derive(clap::Args)]
struct ConnectArgs {
    /// ssh target (user@host): runs `xcomd serve --json` over ssh.
    #[arg(long)]
    ssh: Option<String>,
    /// Extra ssh arguments (e.g. "-p 2222 -i ~/.ssh/key").
    #[arg(long, allow_hyphen_values = true)]
    ssh_args: Option<String>,
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
    /// Windowing backend. `auto` prefers X11/XWayland because per-window
    /// placement and clipboard mirroring depend on it.
    #[arg(long, value_enum, default_value = "auto")]
    backend: Backend,
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
    let wayland = std::env::var_os("WAYLAND_DISPLAY");
    let x11 = std::env::var_os("DISPLAY").is_some();
    let chosen = match choice {
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

/// Run `xcomd serve --json` over ssh and parse the handshake line.
fn bootstrap_ssh(
    target: &str,
    ssh_args: Option<&str>,
    exec: Option<&str>,
) -> Result<Handshake> {
    // Let XCOM_DEBUG follow the session so the daemon traces too.
    let dbg = if xcom_proto::debug() {
        "XCOM_DEBUG=1 "
    } else {
        ""
    };
    let remote = match exec {
        Some(cmd) => format!("{dbg}xcomd serve --json --exec {}", shell_quote(cmd)),
        None => format!("{dbg}xcomd serve --json"),
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
    let Cmd::Connect(args) = cli.cmd;

    // Must run before the event loop (and before any thread) reads the
    // environment.
    let (backend, saved_wayland) = apply_backend(args.backend)?;

    let mut handshake = match (&args.ssh, &args.direct) {
        (Some(target), _) => {
            bootstrap_ssh(target, args.ssh_args.as_deref(), args.exec.as_deref())?
        }
        (None, Some(addr)) => {
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
        "[xcom] connecting to {} (fingerprint {}…)",
        handshake.addr,
        &handshake.fingerprint[..16.min(handshake.fingerprint.len())]
    );

    let timeout = Duration::from_secs(15);
    let control = xcom_transport::connect(
        &handshake.addr,
        Role::Control,
        &handshake.fingerprint,
        &handshake.control_token,
        timeout,
    )
    .context("control channel")?;
    let bulk = xcom_transport::connect(
        &handshake.addr,
        Role::Bulk,
        &handshake.fingerprint,
        &handshake.bulk_token,
        timeout,
    )
    .context("bulk channel")?;
    logln!("[xcom] secure channel established");

    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(el) => el,
        Err(e) => {
            // We may have hidden a perfectly good Wayland session behind a
            // stale DISPLAY; retry there before giving up.
            if backend == Backend::X11 {
                if let Some(wayland) = saved_wayland {
                    logln!("[xcom] X11 backend unavailable ({e}); retrying on Wayland");
                    std::env::set_var("WAYLAND_DISPLAY", wayland);
                    EventLoop::<UserEvent>::with_user_event()
                        .build()
                        .context("creating the event loop (a display server is required for the client)")?
                } else {
                    return Err(e)
                        .context("creating the event loop (a display server is required for the client)");
                }
            } else {
                return Err(e)
                    .context("creating the event loop (a display server is required for the client)");
            }
        }
    };
    let proxy = event_loop.create_proxy();
    // Every message carries its enqueue time: the writer measures how long it
    // waited before reaching the socket (the HUD's queue_ms).
    let (input_tx, input_rx) = mpsc::channel::<(Message, Instant)>();
    let queue_us = Arc::new(AtomicU32::new(0));
    let pending_ping: Arc<Mutex<Option<(u64, Instant)>>> = Arc::new(Mutex::new(None));

    // Control channel: split so input never waits behind a read timeout. The
    // reader blocks on the socket; the writer writes as soon as a message is
    // queued. See xcom_transport::Channel::split.
    let (ctrl_reader, ctrl_writer) = control.split().context("splitting the control channel")?;

    // Reader: window lifecycle in.
    let proxy_reader = proxy.clone();
    let pending_rx = Arc::clone(&pending_ping);
    std::thread::spawn(move || {
        control_reader_loop(ctrl_reader, proxy_reader.clone(), pending_rx);
        // EOF or error: the session is gone — make the UI close its windows
        // instead of leaving stale content on screen.
        proxy_reader.send_event(UserEvent::ConnectionLost).ok();
    });

    // Writer: input out, plus the keepalive. A ping unanswered for 20 s means
    // the peer is gone (sleep/wake, NAT timeout); force-closing the socket
    // wakes the reader so the UI can close the session.
    let queue_tx = Arc::clone(&queue_us);
    let pending_tx = Arc::clone(&pending_ping);
    std::thread::spawn(move || {
        control_writer_loop(ctrl_writer, input_rx, queue_tx, pending_tx);
    });

    // Keepalive + RTT: one ping every 5 s (the first immediately, so the HUD
    // has a round-trip time from the start). The control loop treats 20 s
    // without any traffic from the server as a dead session, which is what
    // turns a sleep/wake or NAT-timeout half-open connection into a closed
    // window instead of a permanently frozen one.
    // Seed the local clipboard from the remote's current selection; after this
    // the daemon pushes changes and the watcher mirrors local ones.
    if !args.no_clipboard_sync {
        input_tx
            .send((
                Message::ClipboardGet {
                    target: xcom_proto::CLIPBOARD_CLIPBOARD,
                },
                Instant::now(),
            ))
            .ok();
    }

    if let Some(text) = args.type_text.clone() {
        let tx = input_tx.clone();
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
                    tx.send((Message::InputKey { sym, pressed }, Instant::now()))
                        .ok();
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

    // Bulk thread: damage frames in.
    let proxy_bulk = proxy.clone();
    std::thread::spawn(move || {
        if let Err(e) = bulk_loop(bulk, proxy_bulk.clone()) {
            logln!("[xcom] bulk channel: {e}");
        }
        proxy_bulk.send_event(UserEvent::ConnectionLost).ok();
    });

    let mut app = App::new(
        proxy,
        input_tx,
        ui::AppOptions {
            dump: args.dump_ppm.clone(),
            fullscreen: args.fullscreen,
            debug_tools: args.debug_tools,
            hud: args.hud,
            queue_us,
            clipboard_sync: !args.no_clipboard_sync,
        },
    );
    let result = event_loop.run_app(&mut app).context("event loop");

    if let Some(mut ssh) = handshake.ssh.take() {
        let _ = ssh.kill();
        let _ = ssh.wait();
    }
    result
}

/// Reader half: blocking, no polling. Dispatches window events to the UI and
/// measures RTT when a pong matches the outstanding ping.
fn control_reader_loop(
    mut reader: xcom_transport::ChannelReader,
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    pending: Arc<Mutex<Option<(u64, Instant)>>>,
) {
    loop {
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

fn control_writer_loop(
    mut writer: xcom_transport::ChannelWriter,
    input_rx: mpsc::Receiver<(Message, Instant)>,
    queue_us: Arc<AtomicU32>,
    pending: Arc<Mutex<Option<(u64, Instant)>>>,
) {
    // First ping immediately, so the HUD has a round-trip time from the start.
    if !send_ping(&mut writer, &pending) {
        writer.force_close();
        return;
    }
    loop {
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

fn bulk_loop(mut bulk: Channel, proxy: winit::event_loop::EventLoopProxy<UserEvent>) -> Result<()> {
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
