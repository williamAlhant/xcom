//! `xcomd serve` — the session daemon.
//!
//! Bootstrap (what makes this "SSH for auth only"): the client runs
//! `ssh user@host 'xcomd serve --json'`; this daemon prints ONE handshake line
//! on stdout (port, single-use token, pinned certificate fingerprint) and then
//! serves the dedicated TLS channel directly — all pixel and input traffic
//! bypasses SSH from that point on.

use std::io::Write;
use std::net::{IpAddr, UdpSocket};
use std::process::{Command, Stdio};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, Window};

use xcom_proto::{logln, Message, MSG_DAMAGE};
use xcom_transport::{random_token, Channel, ChannelAcceptor, Identity, Role};
use xcom_x11::capture::{CaptureConfig, CaptureSession, PumpEvent};
use xcom_x11::clipboard::{Clipboard, Target};
use xcom_x11::input::Injector;
use xcom_x11::keymap::Keymap;
use xcom_x11::windows;
use xcom_x11::xvfb::Xvfb;

#[derive(clap::Args)]
pub struct ServeArgs {
    /// Print the handshake line as JSON on stdout (used by the ssh bootstrap).
    #[arg(long)]
    pub json: bool,
    /// Attach to an existing display (e.g. :9) instead of spawning one.
    /// Diagnostic: capture assumes it owns the display (manual compositing).
    #[arg(long)]
    pub display: Option<String>,
    /// Size of the spawned virtual display, e.g. 1600x1200.
    #[arg(long, default_value = "1600x1200")]
    pub spawn: String,
    /// Listen address for the dedicated channel. `auto` = all interfaces when
    /// bootstrapped over ssh (peer-allowlisted), loopback otherwise.
    #[arg(long, default_value = "auto")]
    pub listen: String,
    /// Accept the TLS channel from any peer, not just the ssh client address.
    /// Only for setups where the client's TLS source address differs (jump
    /// hosts, NAT); it widens the pre-auth exposure.
    #[arg(long)]
    pub allow_any_peer: bool,
    /// Host the client connects to (default: SSH client address, else loopback).
    #[arg(long)]
    pub connect: Option<String>,
    /// Command to launch inside the session (run via sh -c).
    #[arg(long)]
    pub exec: Option<String>,
    /// Seconds to wait for a client to connect (0 = forever).
    #[arg(long, default_value_t = 120)]
    pub accept_timeout: u64,
}

fn parse_size(s: &str) -> Result<(u16, u16)> {
    let (w, h) = s
        .split_once('x')
        .with_context(|| format!("size must look like 1600x1200, got {s:?}"))?;
    Ok((w.parse()?, h.parse()?))
}

/// Address of the ssh client, when this process was started by sshd.
///
/// `SSH_CONNECTION` is `<client ip> <client port> <server ip> <server port>`.
/// The first field is both the peer we expect on the TLS channel and the
/// address the server should advertise for it to dial back to.
fn ssh_client_ip() -> Option<IpAddr> {
    std::env::var("SSH_CONNECTION")
        .ok()
        .and_then(|s| s.split_whitespace().next().and_then(|ip| ip.parse().ok()))
}

/// Best guess at a local address the peer can reach, without sending packets.
fn local_ip_towards(peer: IpAddr) -> Option<IpAddr> {
    let bind = if peer.is_ipv6() { "[::]:0" } else { "0.0.0.0:0" };
    let sock = UdpSocket::bind(bind).ok()?;
    sock.connect((peer, 9)).ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// Format `host:port`, bracketing IPv6 literals.
fn host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Set `PR_SET_PDEATHSIG` so the child dies even if this process is SIGKILLed
/// (a normal `Drop` cannot run then, and an orphaned Xvfb/app would leak).
#[cfg(target_os = "linux")]
fn die_with_parent(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            // Own process group: teardown can kill grandchildren spawned by
            // the shell (e.g. `xterm & xterm & wait`) instead of orphaning them.
            libc::setpgid(0, 0);
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
fn die_with_parent(_cmd: &mut Command) {}

pub fn run(args: ServeArgs) -> Result<()> {
    // --- display ---------------------------------------------------------
    let (display, _xvfb): (String, Option<Xvfb>) = match &args.display {
        Some(d) => (d.clone(), None),
        None => {
            let (w, h) = parse_size(&args.spawn)?;
            let server = Xvfb::spawn(w, h)
                .context("spawning a virtual display (install xorg-server-xvfb)")?;
            (server.display.clone(), Some(server))
        }
    };

    // --- optional app ----------------------------------------------------
    // The app's stdout must never touch ours: stdout is the handshake channel.
    let mut app = match &args.exec {
        Some(cmd) => {
            let mut c = Command::new("sh");
            c.arg("-c")
                .arg(cmd)
                .env("DISPLAY", &display)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit());
            // xterm and command-line tools need a UTF-8 locale to turn
            // non-ASCII keysyms into characters; in a POSIX locale they drop
            // them. Never override a locale the caller set.
            if std::env::var_os("LANG").is_none() && std::env::var_os("LC_ALL").is_none() {
                c.env("LANG", "C.UTF-8");
            }
            die_with_parent(&mut c);
            Some(
                c.spawn()
                    .with_context(|| format!("launching {cmd:?}"))?,
            )
        }
        None => None,
    };

    // --- capture session -------------------------------------------------
    // Only set the root arrow when we own the display; attaching to a real
    // session must not change its cursor.
    let mut session = CaptureSession::connect(
        Some(&display),
        CaptureConfig {
            root_arrow: args.display.is_none(),
            ..Default::default()
        },
    )?;
    let root = session.root();
    for t in windows::list_top_levels(session.connection().as_ref(), root, true)? {
        if t.mapped {
            let _ = session.add_window(t.window)?;
        }
    }

    let keymap = Keymap::query(session.connection().as_ref())?;

    // Clipboard worker: own connection, separate thread; the session still
    // works without it (e.g. if the display refuses another connection).
    let clipboard = match Clipboard::spawn(&display) {
        Ok(c) => Some(c),
        Err(e) => {
            logln!("[serve] clipboard disabled: {e:#}");
            None
        }
    };

    // --- identity, tokens, listener --------------------------------------
    let identity = Identity::generate()?;
    // One single-use token per role: burning the control token cannot make the
    // bulk channel replayable, and a stolen token cannot displace either role.
    let control_token = random_token();
    let bulk_token = random_token();
    let ssh_client = ssh_client_ip();

    let listen = match args.listen.as_str() {
        "auto" => match ssh_client {
            Some(ip) if ip.is_ipv6() => "[::]:0".to_string(),
            Some(_) => "0.0.0.0:0".to_string(),
            None => "127.0.0.1:0".to_string(),
        },
        other => other.to_string(),
    };
    // Bootstrapped over ssh: only the ssh client may complete a handshake.
    let allow_peer = if args.allow_any_peer { None } else { ssh_client };
    let acceptor =
        ChannelAcceptor::bind(&listen, &identity, control_token, bulk_token, allow_peer)?;
    let port = acceptor.local_addr()?.port();

    let host = args
        .connect
        .clone()
        .or_else(|| ssh_client.and_then(local_ip_towards).map(|ip| ip.to_string()))
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let addr = host_port(&host, port);

    // Not derived from token bytes: session ids end up in logs.
    let session_id: u64 = u64::from_le_bytes(random_token()[..8].try_into().unwrap());

    // The single handshake line: everything the client needs to authenticate
    // us (fingerprint) and itself (one token per role), plus where to dial.
    let line = format!(
        "{{\"proto\":{},\"session\":\"{session_id:016x}\",\"display\":\"{display}\",\
         \"connect\":\"{addr}\",\"token_control\":\"{}\",\"token_bulk\":\"{}\",\
         \"cert_sha256\":\"{}\"}}",
        xcom_proto::PROTO_VERSION,
        xcom_transport::hex(&control_token),
        xcom_transport::hex(&bulk_token),
        identity.fingerprint(),
    );
    if args.json {
        // Never panic on a broken stdout: that is the bootstrap channel.
        let mut out = std::io::stdout().lock();
        writeln!(out, "{line}").context("writing the handshake line")?;
        out.flush().context("flushing the handshake line")?;
    } else {
        logln!("[serve] {line}");
    }

    let timeout = Duration::from_secs(if args.accept_timeout == 0 {
        u64::MAX / 2
    } else {
        args.accept_timeout
    });

    logln!("[serve] waiting for client on {addr} (display {display})");
    let control = acceptor
        .accept(Role::Control, timeout)
        .context("accepting control channel")?;
    let mut bulk = acceptor
        .accept(Role::Bulk, timeout)
        .context("accepting bulk channel")?;
    // Both roles are bound to their tokens; no further connections are needed.
    drop(acceptor);
    logln!("[serve] client connected");

    let shutdown = Arc::new(AtomicBool::new(false));

    // --- control thread: input in, window events out ---------------------
    let (ctrl_tx, ctrl_rx) = mpsc::channel::<Message>();
    let (full_tx, full_rx) = mpsc::channel::<Window>();
    let conn_for_input = session.connection();
    let shutdown_ctrl = Arc::clone(&shutdown);
    let geo: GeometryMap = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    for win in session.windows() {
        if let Some((x, y, w, h)) = session.window_geometry(win) {
            geo.lock().unwrap().insert(win, (x, y, w, h));
        }
    }
    let geo_ctrl = Arc::clone(&geo);
    let ctrl_send = ctrl_tx.clone();
    let ctrl_thread = std::thread::spawn(move || {
        if let Err(e) = control_loop(
            control,
            conn_for_input,
            root,
            keymap,
            ctrl_rx,
            ctrl_send,
            full_tx,
            shutdown_ctrl,
            geo_ctrl,
            clipboard,
        ) {
            logln!("[serve] control loop ended with error: {e:#}");
        }
    });

    // --- initial sync: windows that predate the connection ---------------
    // (also covers the "static windows produce no damage" case found in M0:
    // the client always starts from one full frame per window.)
    let existing: Vec<Window> = session.windows().collect();
    let mut announced = std::collections::HashSet::new();
    for win in existing {
        let Some((x, y, w, h)) = session.window_geometry(win) else {
            continue;
        };
        let title =
            windows::window_name(session.connection().as_ref(), win).unwrap_or_default();
        let info = windows::window_info(session.connection().as_ref(), win);
        geo.lock().unwrap().insert(win, (x, y, w, h));
        let msg = Message::WindowCreate {
            id: win,
            x,
            y,
            w,
            h,
            title,
            kind: info.kind,
            parent: info.parent.unwrap_or(0),
            override_redirect: info.override_redirect,
        };
        if ctrl_tx.send(msg).is_err() {
            break;
        }
        announced.insert(win);
        match session.capture_full(win)? {
            Some((frame, _)) => {
                logln!("[serve] sync: full frame {win:#x} ({} rects)", frame.rects.len());
                let payload = frame.encode();
                if bulk.send(MSG_DAMAGE, &payload).is_err() {
                    break;
                }
            }
            None => logln!("[serve] sync: capture_full({win:#x}) returned None"),
        }
    }

    // --- main loop: capture -> bulk channel ------------------------------
    let result = serve_loop(
        &mut session,
        bulk,
        &ctrl_tx,
        &full_rx,
        &shutdown,
        &geo,
        &ctrl_thread,
        announced,
    );

    shutdown.store(true, Ordering::SeqCst);
    if let Some(mut app) = app.take() {
        #[cfg(target_os = "linux")]
        {
            let pid = app.id() as i32;
            // The app is its own process-group leader (setpgid in pre_exec):
            // signal the whole group, then force it after a short grace.
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
            let deadline = Instant::now() + Duration::from_millis(300);
            while Instant::now() < deadline {
                match app.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        let _ = app.kill();
        let _ = app.wait();
    }
    let _ = ctrl_thread.join();
    result
}

/// Sets the session shutdown flag on *every* exit path, including early `?`
/// returns and panics inside the control thread.
struct ShutdownGuard(Arc<AtomicBool>);

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[allow(clippy::too_many_arguments)] // thread entry point, one owner per parameter
fn control_loop(
    mut control: Channel,
    conn: Arc<x11rb::rust_connection::RustConnection>,
    root: Window,
    keymap: Keymap,
    outbound: mpsc::Receiver<Message>,
    outbound_tx: mpsc::Sender<Message>,
    full_requests: mpsc::Sender<Window>,
    shutdown: Arc<AtomicBool>,
    geo: GeometryMap,
    clipboard: Option<Clipboard>,
) -> Result<()> {
    let _guard = ShutdownGuard(Arc::clone(&shutdown));
    let injector = Injector::new(&*conn, root, keymap);
    let mut last_rx = Instant::now();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        // Window events produced by the capture loop.
        while let Ok(msg) = outbound.try_recv() {
            let payload = msg.encode();
            control.send(msg.msg_type(), &payload)?;
        }

        // Poll for inbound input; the short timeout is what lets us service
        // `outbound` promptly. The frame reader keeps partial frames, so this
        // is safe mid-message.
        control.set_read_timeout(Some(Duration::from_millis(5)))?;
        match control.recv() {
            Ok(Some((ty, payload))) => {
                last_rx = Instant::now();
                match Message::decode(ty, &payload)? {
                Message::InputKey { sym, pressed } => {
                    if xcom_proto::debug_keys() {
                        logln!("[serve] recv key sym={sym:#x} pressed={pressed}");
                    }
                    if let Err(e) = injector.key(sym, pressed) {
                        // A single unresolvable keysym must not end the session.
                        logln!("[serve] key {sym:#x} dropped: {e}");
                    }
                }
                Message::Focus { window } => {
                    if !geo.lock().unwrap().contains_key(&window) {
                        logln!("[serve] focus {window:#x}: not a tracked window");
                        continue;
                    }
                    injector.focus(window)?;
                }
                Message::InputButton {
                    window,
                    button,
                    pressed,
                    x,
                    y,
                } => {
                    let Some((rx, ry)) = root_coords(&geo, window, x, y) else {
                        logln!("[serve] button for untracked window {window:#x}");
                        continue;
                    };
                    if xcom_proto::debug() {
                        logln!(
                            "[serve] recv button {button} {pressed} {window:#x} rel=({x},{y}) -> root=({rx},{ry})"
                        );
                    }
                    injector.motion(rx, ry)?;
                    injector.button(button, pressed)?;
                    if pressed && button == 1 {
                        // Click-to-focus (there is no WM on the virtual
                        // display to do it for us).
                        injector.focus(window)?;
                    }
                }
                Message::InputMotion { window, x, y } => {
                    let Some((rx, ry)) = root_coords(&geo, window, x, y) else {
                        logln!("[serve] motion for untracked window {window:#x}");
                        continue;
                    };
                    if xcom_proto::debug() {
                        logln!(
                            "[serve] recv motion {window:#x} rel=({x},{y}) -> root=({rx},{ry}) geo={:?}",
                            geo.lock().unwrap().get(&window)
                        );
                    }
                    injector.motion(rx, ry)?;
                }
                Message::Resize { id, w, h } => {
                    if w == 0 || h == 0 || w > 16384 || h > 16384 {
                        logln!("[serve] resize {id:#x} rejected: {w}x{h}");
                        continue;
                    }
                    if !geo.lock().unwrap().contains_key(&id) {
                        logln!("[serve] resize {id:#x}: not a tracked window");
                        continue;
                    }
                    if xcom_proto::debug() {
                        logln!("[serve] recv resize {id:#x} -> {w}x{h}");
                    }
                    conn.configure_window(
                        id,
                        &xproto::ConfigureWindowAux::new()
                            .width(w as u32)
                            .height(h as u32),
                    )?;
                    conn.flush()?;
                }
                Message::WindowClose { id } => {
                    if !geo.lock().unwrap().contains_key(&id) {
                        logln!("[serve] close {id:#x}: not a tracked window");
                        continue;
                    }
                    match windows::request_close(&*conn, id) {
                        Ok(true) => logln!("[serve] close {id:#x}: WM_DELETE_WINDOW"),
                        Ok(false) => logln!("[serve] close {id:#x}: destroyed (no WM_DELETE_WINDOW)"),
                        Err(e) => logln!("[serve] close {id:#x} failed: {e}"),
                    }
                }
                Message::FullRequest { id } => {
                    full_requests.send(id).ok();
                }
                Message::Ping { t } => {
                    let pong = Message::Pong { t };
                    control.send(pong.msg_type(), &pong.encode())?;
                }
                Message::ClipboardGet { target } => {
                    match (&clipboard, Target::from_u8(target)) {
                        (Some(cb), Some(t)) => {
                            // Read on the worker thread; deliver to the client
                            // through the outbound queue so input never blocks.
                            let (tx, rx) = mpsc::channel();
                            cb.request(t, tx);
                            let out = outbound_tx.clone();
                            std::thread::spawn(move || {
                                let text = rx
                                    .recv_timeout(Duration::from_secs(2))
                                    .ok()
                                    .flatten()
                                    .unwrap_or_default();
                                let msg = Message::ClipboardData {
                                    target: t.as_u8(),
                                    text: xcom_proto::truncate_clipboard(text),
                                };
                                let _ = out.send(msg);
                            });
                        }
                        _ => logln!("[serve] clipboard get: unavailable (target {target})"),
                    }
                }
                Message::ClipboardSet { target, text } => {
                    match (&clipboard, Target::from_u8(target)) {
                        (Some(cb), Some(t)) => {
                            let text = xcom_proto::truncate_clipboard(text);
                            match cb.set(t, text.clone()) {
                                Ok(()) => {
                                    if xcom_proto::debug() {
                                        logln!(
                                            "[serve] clipboard set ({t:?}, {} bytes)",
                                            text.len()
                                        );
                                    }
                                }
                                Err(e) => logln!("[serve] clipboard set failed: {e:#}"),
                            }
                        }
                        _ => logln!("[serve] clipboard set: unavailable (target {target})"),
                    }
                }
                Message::Bye => {
                    shutdown.store(true, Ordering::SeqCst);
                    return Ok(());
                }
                _ => {}
                }
            }

            Ok(None) => {
                logln!("[serve] client left (control channel closed)");
                shutdown.store(true, Ordering::SeqCst);
                return Ok(());
            }
            Err(e) if is_timeout(&e) => {
                if last_rx.elapsed() > Duration::from_secs(30) {
                    logln!("[serve] no input from the client for 30 s; assuming it is gone");
                    return Ok(());
                }
                continue;
            }
            Err(e) if is_eof(&e) => {
                logln!("[serve] client left (EOF)");
                shutdown.store(true, Ordering::SeqCst);
                return Ok(());
            }
            Err(e) => {
                logln!("[serve] control channel error: {e}");
                shutdown.store(true, Ordering::SeqCst);
                return Ok(());
            }
        }
    }
}

/// Window geometry (root x/y plus size) shared between the capture loop and
/// the input path: XTEST injects in root space, so pointer events sent relative
/// to a window are translated with its current position and clamped to its
/// current size.
type GeometryMap = Arc<std::sync::Mutex<std::collections::HashMap<Window, (i16, i16, u16, u16)>>>;

fn root_coords(geo: &GeometryMap, window: Window, x: i16, y: i16) -> Option<(i16, i16)> {
    let map = geo.lock().unwrap();
    let (wx, wy, w, h) = *map.get(&window)?;
    let cx = (x.max(0) as i32).min(w.max(1) as i32 - 1) as i16;
    let cy = (y.max(0) as i32).min(h.max(1) as i32 - 1) as i16;
    Some((wx.saturating_add(cx), wy.saturating_add(cy)))
}

/// Peer closed the connection (possibly without TLS close_notify).
fn is_eof(e: &xcom_transport::TransportError) -> bool {
    use xcom_transport::TransportError as T;
    match e {
        T::Framing(xcom_proto::ProtoError::Io(io)) | T::Io(io) => {
            io.kind() == std::io::ErrorKind::UnexpectedEof
                || io.kind() == std::io::ErrorKind::ConnectionReset
                || io.kind() == std::io::ErrorKind::BrokenPipe
        }
        T::Tls(tls) => {
            // rustls reports a missing close_notify as an io-style error string.
            let s = tls.to_string();
            s.contains("close_notify") || s.contains("unexpected eof")
        }
        _ => false,
    }
}

fn is_timeout(e: &xcom_transport::TransportError) -> bool {
    use xcom_transport::TransportError as T;
    let io = match e {
        T::Framing(xcom_proto::ProtoError::Io(io)) | T::Io(io) => io,
        _ => return false,
    };
    io.kind() == std::io::ErrorKind::WouldBlock || io.kind() == std::io::ErrorKind::TimedOut
}

/// Bookkeeping for the post-paint full sync.
///
/// A window's pixmap is undefined until the app paints it (toolkits like GTK
/// draw everything themselves; map-time captures can read recycled server
/// memory), and a resize leaves newly exposed regions undefined the same way.
/// Damage frames only ever cover the rects the app drew, so garbage outside
/// them would survive forever: after every full sync, re-sync once more a bit
/// later, unconditionally, to overwrite it with what the app has painted.
struct SyncState {
    last_full: Instant,
    attempts: u8,
}

#[allow(clippy::too_many_arguments)] // main loop state, one owner per parameter
fn serve_loop(
    session: &mut CaptureSession,
    mut bulk: Channel,
    ctrl_tx: &mpsc::Sender<Message>,
    full_rx: &mpsc::Receiver<Window>,
    shutdown: &AtomicBool,
    geo: &GeometryMap,
    ctrl_thread: &std::thread::JoinHandle<()>,
    mut announced: std::collections::HashSet<Window>,
) -> Result<()> {
    // A window's first full frame can be captured before the app has painted
    // (and a freshly named pixmap may hold recycled memory). Static apps never
    // send damage to correct it, so re-sync any window that produced no damage
    // shortly after its full frame — a couple of attempts, not an endless poll.
    let mut sync: HashMap<Window, SyncState> = session
        .windows()
        .map(|w| {
            (
                w,
                SyncState {
                    last_full: Instant::now(),
                    attempts: 1,
                },
            )
        })
        .collect();

    let mut stats_frames = 0u64;
    let mut stats_raw = 0u64;
    let mut stats_comp = 0u64;
    let mut last_report = Instant::now();
    let mut last_frame = Instant::now() - Duration::from_secs(1);

    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(());
        }
        // The control thread owns input and lifecycle; if it is gone the
        // session is dead even if the bulk socket still looks writable.
        if ctrl_thread.is_finished() {
            logln!("[serve] control thread exited; ending the session");
            shutdown.store(true, Ordering::SeqCst);
            return Ok(());
        }

        // Full-refresh requests (attach, resizes, client-side repair).
        while let Ok(win) = full_rx.try_recv() {
            if let Some((frame, _)) = session.capture_full(win)? {
                let payload = frame.encode();
                if bulk.send(MSG_DAMAGE, &payload).is_err() {
                    shutdown.store(true, Ordering::SeqCst);
                    return Ok(());
                }
            }
            sync.insert(
                win,
                SyncState {
                    last_full: Instant::now(),
                    attempts: 1,
                },
            );
        }

        // Post-paint re-syncs (see SyncState): overwrite undefined pixmap
        // regions with whatever the app has painted in the meantime.
        let stale: Vec<Window> = sync
            .iter()
            .filter(|(_, s)| s.attempts < 3 && s.last_full.elapsed() >= Duration::from_millis(400))
            .map(|(w, _)| *w)
            .collect();
        for win in stale {
            if let Some(st) = sync.get_mut(&win) {
                st.attempts += 1;
                st.last_full = Instant::now();
            }
            if let Some((frame, _)) = session.capture_full(win)? {
                let payload = frame.encode();
                if bulk.send(MSG_DAMAGE, &payload).is_err() {
                    shutdown.store(true, Ordering::SeqCst);
                    return Ok(());
                }
            }
        }

        // Latest-wins pacing: cap captures at ~120 fps. Damage keeps
        // coalescing in the X server while we sleep, so nothing is lost.
        const MIN_FRAME_INTERVAL: Duration = Duration::from_millis(8);
        if let Some(rest) = MIN_FRAME_INTERVAL.checked_sub(last_frame.elapsed()) {
            std::thread::sleep(rest);
        }

        match session.pump(Some(Instant::now() + Duration::from_millis(200)))? {
            Some(PumpEvent::Frame { frame, stats, .. }) => {
                if xcom_proto::debug() {
                    logln!(
                        "[serve] frame {:#x} {}x{} rects={:?}",
                        frame.window,
                        frame.win_w,
                        frame.win_h,
                        frame.rects.iter().map(|r| (r.x, r.y, r.w, r.h)).collect::<Vec<_>>()
                    );
                }
                let payload = frame.encode();
                if bulk.send(MSG_DAMAGE, &payload).is_err() {
                    logln!("[serve] bulk channel closed");
                    shutdown.store(true, Ordering::SeqCst);
                    return Ok(());
                }
                stats_frames += 1;
                stats_raw += stats.raw_bytes as u64;
                stats_comp += stats.comp_bytes as u64;
                last_frame = Instant::now();
            }
            Some(PumpEvent::Mapped(win)) => {
                if let Some((x, y, w, h)) = session.window_geometry(win) {
                    let mut info = windows::window_info(session.connection().as_ref(), win);
                    // Menus/tooltips often omit WM_TRANSIENT_FOR. Their parent
                    // is, for placement purposes, the window that had focus
                    // when they popped up.
                    if info.parent.is_none() && info.override_redirect {
                        if let Some(focus) = session
                            .connection()
                            .get_input_focus()
                            .ok()
                            .and_then(|c| c.reply().ok())
                            .map(|r| r.focus)
                        {
                            if session.tracked(focus) {
                                info.parent = Some(focus);
                            }
                        }
                    }
                    // No WM on this display: give new windows keyboard focus,
                    // or XTEST keystrokes go to whatever the pointer is over.
                    // Override-redirect popups (menus, tooltips) rely on grabs
                    // and must not have focus moved to them.
                    if !info.override_redirect {
                        session
                            .connection()
                            .set_input_focus(
                                xproto::InputFocus::PARENT,
                                win,
                                xproto::Timestamp::from(0u32),
                            )
                            .map_err(|e| anyhow::anyhow!("set_input_focus: {e}"))?;
                        session.connection().flush().ok();
                    }
                    geo.lock().unwrap().insert(win, (x, y, w, h));
                    if announced.insert(win) {
                        let title = windows::window_name(
                            session.connection().as_ref(),
                            win,
                        )
                        .unwrap_or_default();
                        if xcom_proto::debug() {
                            logln!(
                                "[serve] mapped {win:#x} kind={} parent={:?} override={}",
                                info.kind,
                                info.parent,
                                info.override_redirect
                            );
                        }
                        ctrl_tx
                            .send(Message::WindowCreate {
                                id: win,
                                x,
                                y,
                                w,
                                h,
                                title,
                                kind: info.kind,
                                parent: info.parent.unwrap_or(0),
                                override_redirect: info.override_redirect,
                            })
                            .ok();
                    } else {
                        // Unmapped then mapped again: the client still has the
                        // window and its store, it only needs to show it.
                        ctrl_tx.send(Message::WindowMapped { id: win }).ok();
                    }
                    // A freshly mapped window has no damage history: send one
                    // full frame so static windows (xterm idle, dialogs) show
                    // up immediately. On remap this also repairs everything
                    // drawn while the window was hidden.
                    sync.insert(
                        win,
                        SyncState {
                            last_full: Instant::now(),
                            attempts: 1,
                        },
                    );
                    match session.capture_full(win)? {
                        Some((frame, _)) => {
                            logln!("[serve] mapped: full frame {win:#x} ({} rects)", frame.rects.len());
                            let payload = frame.encode();
                            if bulk.send(MSG_DAMAGE, &payload).is_err() {
                                shutdown.store(true, Ordering::SeqCst);
                                return Ok(());
                            }
                        }
                        None => logln!("[serve] mapped: capture_full({win:#x}) returned None"),
                    }
                }
            }
            Some(PumpEvent::Unmapped(win)) => {
                // Not a destroy: the app may map the window again. Keep the
                // geometry (it still exists) but stop scheduled re-syncs.
                sync.remove(&win);
                ctrl_tx.send(Message::WindowUnmapped { id: win }).ok();
            }
            Some(PumpEvent::Destroyed(win)) => {
                geo.lock().unwrap().remove(&win);
                sync.remove(&win);
                announced.remove(&win);
                ctrl_tx.send(Message::WindowDestroy { id: win }).ok();
            }
            Some(PumpEvent::Cursor {
                xhot,
                yhot,
                w,
                h,
                pixels,
            }) => {
                if xcom_proto::debug() {
                    logln!("[serve] cursor {w}x{h} hot=({xhot},{yhot})");
                }
                ctrl_tx
                    .send(Message::Cursor {
                        xhot,
                        yhot,
                        w,
                        h,
                        pixels,
                    })
                    .ok();
            }
            Some(PumpEvent::Resized { window, w, h }) => {
                let (x, y) = if let Some((x, y, _, _)) = session.window_geometry(window) {
                    geo.lock().unwrap().insert(window, (x, y, w, h));
                    (x, y)
                } else {
                    (0, 0)
                };
                ctrl_tx
                    .send(Message::WindowGeometry {
                        id: window,
                        x,
                        y,
                        w,
                        h,
                    })
                    .ok();
                // Newly exposed regions are undefined until the app repaints:
                // schedule post-paint re-syncs like on map.
                sync.insert(
                    window,
                    SyncState {
                        last_full: Instant::now(),
                        attempts: 1,
                    },
                );
            }
            None => {}
        }

        if last_report.elapsed() >= Duration::from_secs(5) && stats_frames > 0 {
            logln!(
                "[serve] frames={stats_frames} raw={:.0}MB comp={:.1}MB ratio={:.1}x",
                stats_raw as f64 / 1e6,
                stats_comp as f64 / 1e6,
                if stats_comp > 0 {
                    stats_raw as f64 / stats_comp as f64
                } else {
                    0.0
                }
            );
            last_report = Instant::now();
        }
    }
}
