//! `xcomd serve` — the session daemon.
//!
//! Bootstrap (what makes this "SSH for auth only"): the client runs
//! `ssh user@host 'xcomd serve --json'`; this daemon prints ONE handshake line
//! on stdout (port, single-use token, pinned certificate fingerprint) and then
//! serves the dedicated TLS channel directly — all pixel and input traffic
//! bypasses SSH from that point on.

use std::io::Write;
use std::process::{Command, Stdio};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, Window};

use xcom_proto::{Message, MSG_DAMAGE};
use xcom_transport::{random_token, Channel, ChannelAcceptor, Identity, Role};
use xcom_x11::capture::{CaptureConfig, CaptureSession, PumpEvent};
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
    #[arg(long)]
    pub display: Option<String>,
    /// Size of the spawned virtual display, e.g. 1600x1200.
    #[arg(long, default_value = "1600x1200")]
    pub spawn: String,
    /// Listen address for the dedicated channel (port 0 = pick one).
    #[arg(long, default_value = "0.0.0.0:0")]
    pub listen: String,
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

/// Best guess at the address the client should dial back to: the peer address
/// of our own ssh session (containers/NAT may need `xcom connect --connect`).
fn ssh_peer_host() -> Option<String> {
    std::env::var("SSH_CONNECTION")
        .ok()
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
}

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
        Some(cmd) => Some(
            Command::new("sh")
                .arg("-c")
                .arg(cmd)
                .env("DISPLAY", &display)
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .spawn()
                .with_context(|| format!("launching {cmd:?}"))?,
        ),
        None => None,
    };

    // --- capture session -------------------------------------------------
    let mut session = CaptureSession::connect(Some(&display), CaptureConfig::default())?;
    let root = session.root();
    for t in windows::list_top_levels(session.connection().as_ref(), root, true)? {
        if t.mapped {
            session.add_window(t.window)?;
        }
    }

    let keymap = Keymap::query(session.connection().as_ref())?;

    // --- identity, token, listener ---------------------------------------
    let identity = Identity::generate()?;
    let token = random_token();
    let acceptor = ChannelAcceptor::bind(&args.listen, &identity, token)?;
    let port = acceptor.local_addr()?.port();
    let host = args
        .connect
        .clone()
        .or_else(ssh_peer_host)
        .unwrap_or_else(|| "127.0.0.1".to_string());

    let session_id: u64 = u64::from_le_bytes(token[..8].try_into().unwrap());

    // The single handshake line: everything the client needs to authenticate
    // us (fingerprint) and itself (token), plus where to dial.
    let line = format!(
        "{{\"proto\":1,\"session\":\"{session_id:016x}\",\"display\":\"{display}\",\
         \"connect\":\"{host}:{port}\",\"token\":\"{}\",\"cert_sha256\":\"{}\"}}",
        xcom_transport::hex(&token),
        identity.fingerprint(),
    );
    if args.json {
        println!("{line}");
    } else {
        eprintln!("[serve] {line}");
    }
    std::io::stdout().flush()?;
    std::io::stderr().flush()?;

    let timeout = Duration::from_secs(if args.accept_timeout == 0 {
        u64::MAX / 2
    } else {
        args.accept_timeout
    });

    eprintln!("[serve] waiting for client on {host}:{port} (display {display})");
    let control = acceptor
        .accept(Role::Control, timeout)
        .context("accepting control channel")?;
    let mut bulk = acceptor
        .accept(Role::Bulk, timeout)
        .context("accepting bulk channel")?;
    eprintln!("[serve] client connected");

    let shutdown = Arc::new(AtomicBool::new(false));

    // --- control thread: input in, window events out ---------------------
    let (ctrl_tx, ctrl_rx) = mpsc::channel::<Message>();
    let (full_tx, full_rx) = mpsc::channel::<Window>();
    let conn_for_input = session.connection();
    let shutdown_ctrl = Arc::clone(&shutdown);
    let geo: GeometryMap = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    for win in session.windows() {
        if let Some((x, y, _, _)) = session.window_geometry(win) {
            geo.lock().unwrap().insert(win, (x, y));
        }
    }
    let geo_ctrl = Arc::clone(&geo);
    let ctrl_thread = std::thread::spawn(move || {
        control_loop(
            control,
            conn_for_input,
            root,
            keymap,
            ctrl_rx,
            full_tx,
            shutdown_ctrl,
            geo_ctrl,
        )
    });

    // --- initial sync: windows that predate the connection ---------------
    // (also covers the "static windows produce no damage" case found in M0:
    // the client always starts from one full frame per window.)
    let existing: Vec<Window> = session.windows().collect();
    for win in existing {
        let Some((x, y, w, h)) = session.window_geometry(win) else {
            continue;
        };
        let title =
            windows::window_name(session.connection().as_ref(), win).unwrap_or_default();
        geo.lock().unwrap().insert(win, (x, y));
        let msg = Message::WindowCreate {
            id: win,
            x,
            y,
            w,
            h,
            title,
        };
        if ctrl_tx.send(msg).is_err() {
            break;
        }
        match session.capture_full(win)? {
            Some((frame, _)) => {
                eprintln!("[serve] sync: full frame {win:#x} ({} rects)", frame.rects.len());
                let payload = frame.encode();
                if bulk.send(MSG_DAMAGE, &payload).is_err() {
                    break;
                }
            }
            None => eprintln!("[serve] sync: capture_full({win:#x}) returned None"),
        }
    }

    // --- main loop: capture -> bulk channel ------------------------------
    let result = serve_loop(&mut session, bulk, &ctrl_tx, &full_rx, &shutdown, &geo);

    shutdown.store(true, Ordering::SeqCst);
    if let Some(mut app) = app.take() {
        let _ = app.kill();
        let _ = app.wait();
    }
    let _ = ctrl_thread.join();
    result
}

fn control_loop(
    mut control: Channel,
    conn: Arc<x11rb::rust_connection::RustConnection>,
    root: Window,
    keymap: Keymap,
    outbound: mpsc::Receiver<Message>,
    full_requests: mpsc::Sender<Window>,
    shutdown: Arc<AtomicBool>,
    geo: GeometryMap,
) -> Result<()> {
    let injector = Injector::new(&*conn, root, keymap);
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
            Ok(Some((ty, payload))) => match Message::decode(ty, &payload)? {
                Message::InputKey { sym, pressed } => {
                    if xcom_proto::debug() {
                        eprintln!("[serve] recv key sym={sym:#x} pressed={pressed}");
                    }
                    injector.key(sym, pressed)?;
                }
                Message::Focus { window } => {
                    if xcom_proto::debug() {
                        eprintln!("[serve] recv focus {window:#x}");
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
                    let (rx, ry) = root_coords(&geo, window, x, y);
                    if xcom_proto::debug() {
                        eprintln!(
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
                    let (rx, ry) = root_coords(&geo, window, x, y);
                    if xcom_proto::debug() {
                        eprintln!(
                            "[serve] recv motion {window:#x} rel=({x},{y}) -> root=({rx},{ry}) geo={:?}",
                            geo.lock().unwrap().get(&window)
                        );
                    }
                    injector.motion(rx, ry)?;
                }
                Message::Resize { id, w, h } => {
                    if xcom_proto::debug() {
                        eprintln!("[serve] recv resize {id:#x} -> {w}x{h}");
                    }
                    conn.configure_window(
                        id,
                        &xproto::ConfigureWindowAux::new()
                            .width(w as u32)
                            .height(h as u32),
                    )?;
                    conn.flush()?;
                }
                Message::FullRequest { id } => {
                    full_requests.send(id).ok();
                }
                Message::Ping { t } => {
                    let pong = Message::Pong { t };
                    control.send(pong.msg_type(), &pong.encode())?;
                }
                Message::Bye => {
                    shutdown.store(true, Ordering::SeqCst);
                    return Ok(());
                }
                _ => {}
            },
            Ok(None) => {
                eprintln!("[serve] client left (control channel closed)");
                shutdown.store(true, Ordering::SeqCst);
                return Ok(());
            }
            Err(e) if is_timeout(&e) => continue,
            Err(e) if is_eof(&e) => {
                eprintln!("[serve] client left (EOF)");
                shutdown.store(true, Ordering::SeqCst);
                return Ok(());
            }
            Err(e) => {
                eprintln!("[serve] control channel error: {e}");
                shutdown.store(true, Ordering::SeqCst);
                return Ok(());
            }
        }
    }
}

/// Window geometry (root coords) shared between the capture loop and the
/// input path: XTEST injects in root space, so pointer events sent relative to
/// a window must be translated with the window's current position.
type GeometryMap = Arc<std::sync::Mutex<std::collections::HashMap<Window, (i16, i16)>>>;

fn root_coords(geo: &GeometryMap, window: Window, x: i16, y: i16) -> (i16, i16) {
    let (wx, wy) = geo
        .lock()
        .unwrap()
        .get(&window)
        .copied()
        .unwrap_or((0, 0));
    (wx.saturating_add(x), wy.saturating_add(y))
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

fn serve_loop(
    session: &mut CaptureSession,
    mut bulk: Channel,
    ctrl_tx: &mpsc::Sender<Message>,
    full_rx: &mpsc::Receiver<Window>,
    shutdown: &AtomicBool,
    geo: &GeometryMap,
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

    loop {
        if shutdown.load(Ordering::SeqCst) {
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

        match session.pump(Some(Instant::now() + Duration::from_millis(200)))? {
            Some(PumpEvent::Frame { frame, stats, .. }) => {
                let payload = frame.encode();
                if bulk.send(MSG_DAMAGE, &payload).is_err() {
                    eprintln!("[serve] bulk channel closed");
                    shutdown.store(true, Ordering::SeqCst);
                    return Ok(());
                }
                stats_frames += 1;
                stats_raw += stats.raw_bytes as u64;
                stats_comp += stats.comp_bytes as u64;
            }
            Some(PumpEvent::Mapped(win)) => {
                if let Some((x, y, w, h)) = session.window_geometry(win) {
                    // No WM on this display: give new windows keyboard focus,
                    // or XTEST keystrokes go to whatever the pointer is over.
                    session
                        .connection()
                        .set_input_focus(
                            xproto::InputFocus::PARENT,
                            win,
                            xproto::Timestamp::from(0u32),
                        )
                        .map_err(|e| anyhow::anyhow!("set_input_focus: {e}"))?;
                    session.connection().flush().ok();
                    let title = windows::window_name(session.connection().as_ref(), win)
                        .unwrap_or_default();
                    geo.lock().unwrap().insert(win, (x, y));
                    ctrl_tx
                        .send(Message::WindowCreate {
                            id: win,
                            x,
                            y,
                            w,
                            h,
                            title,
                        })
                        .ok();
                    // A freshly mapped window has no damage history: send one
                    // full frame so static windows (xterm idle, dialogs) show
                    // up immediately.
                    sync.insert(
                        win,
                        SyncState {
                            last_full: Instant::now(),
                            attempts: 1,
                        },
                    );
                    match session.capture_full(win)? {
                        Some((frame, _)) => {
                            eprintln!("[serve] mapped: full frame {win:#x} ({} rects)", frame.rects.len());
                            let payload = frame.encode();
                            if bulk.send(MSG_DAMAGE, &payload).is_err() {
                                shutdown.store(true, Ordering::SeqCst);
                                return Ok(());
                            }
                        }
                        None => eprintln!("[serve] mapped: capture_full({win:#x}) returned None"),
                    }
                }
            }
            Some(PumpEvent::Unmapped(win)) | Some(PumpEvent::Destroyed(win)) => {
                geo.lock().unwrap().remove(&win);
                ctrl_tx.send(Message::WindowDestroy { id: win }).ok();
            }
            Some(PumpEvent::Resized { window, w, h }) => {
                if let Some((x, y, _, _)) = session.window_geometry(window) {
                    geo.lock().unwrap().insert(window, (x, y));
                }
                ctrl_tx.send(Message::WindowGeometry { id: window, w, h }).ok();
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
            eprintln!(
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
