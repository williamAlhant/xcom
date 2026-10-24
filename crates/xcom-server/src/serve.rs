//! `xcomd serve` — the session daemon.
//!
//! Bootstrap (what makes this "SSH for auth only"): the client runs
//! `ssh user@host 'xcomd serve --json'`; this daemon prints ONE handshake line
//! on stdout (port, single-use token, pinned certificate fingerprint) and then
//! serves the dedicated TLS channel directly — all pixel and input traffic
//! bypasses SSH from that point on.

use std::io::Write;
use std::net::{IpAddr, UdpSocket};
use std::process::{Child, Command, Stdio};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _, Window};

use xcom_proto::{logln, Message, MSG_DAMAGE};
use xcom_transport::{random_token, Channel, ChannelAcceptor, Identity, Role};
use xcom_x11::capture::{CaptureConfig, CaptureSession, FrameStats, PumpEvent};
use xcom_x11::clipboard::{Clipboard, Target};
use xcom_x11::input::Injector;
use xcom_x11::keymap::Keymap;
use xcom_x11::windows;
use xcom_x11::xvfb::Xvfb;

#[derive(clap::Args)]
pub struct DisplayArgs {
    /// Attach to an existing display (e.g. :9) instead of spawning one.
    /// Diagnostic: capture assumes it owns the display (manual compositing).
    #[arg(long)]
    pub display: Option<String>,
    /// Size of the spawned virtual display, e.g. 1600x1200.
    #[arg(long, default_value = "1600x1200")]
    pub spawn: String,
    /// Command to launch inside the session (run via sh -c).
    #[arg(long)]
    pub exec: Option<String>,
}

#[derive(clap::Args)]
pub struct NetArgs {
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
    /// Seconds to wait for a client to connect (0 = forever).
    #[arg(long, default_value_t = 120)]
    pub accept_timeout: u64,
}

#[derive(clap::Args)]
pub struct ServeArgs {
    #[command(flatten)]
    pub display: DisplayArgs,
    #[command(flatten)]
    pub net: NetArgs,
    /// Print the handshake line as JSON on stdout (used by the ssh bootstrap).
    #[arg(long)]
    pub json: bool,
    /// Test aid: drop the client link after N seconds without ending the
    /// session (the reconnect e2e uses it).
    #[arg(long, default_value_t = 0)]
    pub drop_after: u64,
}

#[derive(clap::Args)]
pub struct SessionArgs {
    #[command(flatten)]
    pub display: DisplayArgs,
    #[command(flatten)]
    pub net: NetArgs,
    /// Session name: the rendezvous socket and `--session` on the client.
    #[arg(long, default_value = crate::control::DEFAULT_SESSION)]
    pub name: String,
    /// End the session after this many seconds without an attached client
    /// (0 = never).
    #[arg(long, default_value_t = 0)]
    pub idle_timeout: u64,
    /// Test aid: drop each client link after N seconds without ending the
    /// session (the reconnect e2e uses it).
    #[arg(long, default_value_t = 0)]
    pub drop_after: u64,
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
pub(crate) fn ssh_client_ip() -> Option<IpAddr> {
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

/// Stable queue for clipboard pushes from the owner-owned worker.
///
/// The clipboard must outlive attachments (it owns the X selection and serves
/// paste requests), so the receiver lives in the owner and each attachment
/// drains it. Messages produced while detached are discarded by the owner.
type ClipboardQueue = Arc<std::sync::Mutex<mpsc::Receiver<Message>>>;

/// Long-lived session state: display, app and capture pipeline.
///
/// Attachments come and go; everything here survives a client leaving.
struct Owner {
    name: String,
    session_id: u64,
    created: u64,
    /// The app command line, for `sessions` and the handshake.
    app_cmd: String,
    display: String,
    session: CaptureSession,
    keymap: Keymap,
    app: Option<Child>,
    _xvfb: Option<Xvfb>,
    geo: GeometryMap,
    screen: (u16, u16),
    clipboard: Option<Clipboard>,
    clipboard_rx: ClipboardQueue,
    /// Test aid: seconds after which each attachment is dropped (0 = never).
    drop_after: u64,
}

impl Owner {
    fn start(display_args: &DisplayArgs, name: &str, drop_after: u64) -> Result<Owner> {
        crate::control::validate_name(name)?;
        // --- display -----------------------------------------------------
        let (display, xvfb): (String, Option<Xvfb>) = match &display_args.display {
            Some(d) => (d.clone(), None),
            None => {
                let (w, h) = parse_size(&display_args.spawn)?;
                let server = Xvfb::spawn(w, h)
                    .context("spawning a virtual display (install xorg-server-xvfb)")?;
                (server.display.clone(), Some(server))
            }
        };

        // --- optional app ------------------------------------------------
        // The app's stdout must never touch ours: stdout is the handshake channel.
        let app = match &display_args.exec {
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

        // --- capture session ---------------------------------------------
        // Only set the root arrow when we own the display; attaching to a real
        // session must not change its cursor.
        let mut session = CaptureSession::connect(
            Some(&display),
            CaptureConfig {
                root_arrow: display_args.display.is_none(),
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
        let screen = session.screen_size();

        // Geometry is shared between the capture loop and input injection.
        let geo: GeometryMap = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        for win in session.windows() {
            if let Some(g) = session.window_geometry(win) {
                geo.lock().unwrap().insert(win, g);
            }
        }

        // One clipboard worker for the session: it owns the remote selection,
        // so it must survive detach. Its queue is drained by the current
        // attachment; the owner discards messages while detached.
        let (clip_tx, clip_rx) = mpsc::channel();
        let clipboard = match Clipboard::spawn(&display, clip_tx) {
            Ok(c) => Some(c),
            Err(e) => {
                logln!("[serve] clipboard disabled: {e:#}");
                None
            }
        };

        Ok(Owner {
            name: name.to_string(),
            session_id: u64::from_le_bytes(random_token()[..8].try_into().unwrap()),
            created: now_unix(),
            app_cmd: display_args.exec.clone().unwrap_or_default(),
            display,
            session,
            keymap,
            app,
            _xvfb: xvfb,
            geo,
            screen,
            clipboard,
            clipboard_rx: Arc::new(std::sync::Mutex::new(clip_rx)),
            drop_after,
        })
    }

    /// Bind the TLS acceptor and build the handshake line for one attachment.
    fn prepare_attachment(&self, net: &NetArgs, peer: Option<IpAddr>) -> Result<Attachment> {
        let identity = Identity::generate()?;
        // One single-use token per role: burning the control token cannot make
        // the bulk channel replayable, and a stolen token cannot displace either
        // role.
        let control_token = random_token();
        let bulk_token = random_token();

        let listen = match net.listen.as_str() {
            "auto" => match peer {
                Some(ip) if ip.is_ipv6() => "[::]:0".to_string(),
                Some(_) => "0.0.0.0:0".to_string(),
                None => "127.0.0.1:0".to_string(),
            },
            other => other.to_string(),
        };
        // Bootstrapped over ssh: only the ssh client may complete a handshake.
        let allow_peer = if net.allow_any_peer { None } else { peer };
        let acceptor =
            ChannelAcceptor::bind(&listen, &identity, control_token, bulk_token, allow_peer)?;
        ensure_peer_policy(allow_peer, net.allow_any_peer, acceptor.local_addr()?.ip())?;
        let port = acceptor.local_addr()?.port();

        let host = net
            .connect
            .clone()
            .or_else(|| peer.and_then(local_ip_towards).map(|ip| ip.to_string()))
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let addr = host_port(&host, port);

        // The single handshake line: everything the client needs to authenticate
        // us (fingerprint) and itself (one token per role), plus where to dial.
        let session_id = self.session_id;
        let line = format!(
            "{{\"ok\":true,\"proto\":{},\"build\":\"{}\",\"session\":\"{session_id:016x}\",\
             \"session_name\":\"{}\",\"created\":{},\"attached\":true,\
             \"display\":\"{}\",\"app\":\"{}\",\"connect\":\"{addr}\",\
             \"token_control\":\"{}\",\"token_bulk\":\"{}\",\
             \"cert_sha256\":\"{}\"}}",
            xcom_proto::PROTO_VERSION,
            xcom_proto::BUILD,
            self.name,
            self.created,
            self.display,
            crate::control::json_escape(&self.app_cmd),
            xcom_transport::hex(&control_token),
            xcom_transport::hex(&bulk_token),
            identity.fingerprint(),
        );
        Ok(Attachment {
            acceptor,
            line,
            addr,
        })
    }
}

/// Without an ssh peer there is nobody to allowlist. Loopback is still safe;
/// anything else would silently expose the TLS port, so refuse unless the
/// operator explicitly opted into `--allow-any-peer`.
fn ensure_peer_policy(allow_peer: Option<IpAddr>, allow_any: bool, bound: IpAddr) -> Result<()> {
    if allow_peer.is_none() && !allow_any && !bound.is_loopback() {
        bail!(
            "no ssh peer to allowlist: refusing to listen on {bound}; run the \
             bootstrap over ssh or pass --allow-any-peer"
        );
    }
    Ok(())
}

/// One prepared client rendezvous: bound TLS acceptor plus wire handshake.
struct Attachment {
    acceptor: ChannelAcceptor,
    line: String,
    addr: String,
}

/// How one attachment ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttachmentEnd {
    /// The client left, crashed or was disconnected; the session lives on.
    Detached,
    /// The remote app exited: the session is over.
    AppExited,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Accept the two TLS channels and serve one client until it leaves.
fn serve_attachment(
    owner: &mut Owner,
    att: &Attachment,
    net: &NetArgs,
    session_end: &AtomicBool,
) -> Result<AttachmentEnd> {
    let Owner {
        display,
        session,
        keymap,
        geo,
        screen,
        clipboard,
        clipboard_rx,
        app,
        drop_after,
        ..
    } = owner;
    let (screen_w, screen_h) = *screen;
    let root = session.root();

    let timeout = Duration::from_secs(if net.accept_timeout == 0 {
        u64::MAX / 2
    } else {
        net.accept_timeout
    });
    logln!(
        "[serve] waiting for client on {} (display {display}, {screen_w}x{screen_h})",
        att.addr
    );
    let mut control = att
        .acceptor
        .accept(Role::Control, timeout)
        .context("accepting control channel")?;
    // The second role must follow promptly. A partial connect (control up,
    // bulk fails) otherwise keeps the session "busy" for the whole
    // --accept-timeout, which kills the client's reconnect.
    const SECOND_ROLE_WINDOW: Duration = Duration::from_secs(10);
    let mut bulk = match att
        .acceptor
        .accept(Role::Bulk, timeout.min(SECOND_ROLE_WINDOW))
    {
        Ok(bulk) => bulk,
        Err(e) => {
            logln!("[serve] bulk channel did not arrive ({e}); detaching");
            return Err(e).context("accepting bulk channel");
        }
    };
    logln!("[serve] client connected");

    // A new client has no cursor state: force the current image through even
    // if its serial has not changed since the previous attachment.
    if let Err(e) = session.refresh_cursor() {
        logln!("[serve] cursor refresh failed: {e:#}");
    }

    // Announce the display first: the client caps windows at it. The X pointer
    // cannot leave the screen, so a larger window would have a region that is
    // visible but unclickable (XTEST clamps at the screen edge).
    let display_msg = Message::Display {
        w: screen_w,
        h: screen_h,
    };
    control.send(display_msg.msg_type(), &display_msg.encode())?;

    let shutdown = Arc::new(AtomicBool::new(false));

    // --- control thread: input in, window events out ---------------------
    let (ctrl_tx, ctrl_rx) = mpsc::channel::<Message>();

    let (full_tx, full_rx) = mpsc::channel::<Window>();
    let conn_for_input = session.connection();
    let shutdown_ctrl = Arc::clone(&shutdown);
    // Set when the *session* is ending (app exit or kill), as opposed to the
    // client just leaving. The control thread uses it to send a final
    // SessionEnded so the client does not try to reconnect.
    let session_ended = Arc::new(AtomicBool::new(false));
    let session_ended_ctrl = Arc::clone(&session_ended);
    let geo_ctrl = Arc::clone(geo);
    let clipboard_ctrl = clipboard.clone();
    let clipboard_rx_ctrl = Arc::clone(clipboard_rx);
    let keymap_ctrl = Keymap::clone(keymap);
    let ctrl_send = ctrl_tx.clone();
    let ctrl_thread = std::thread::spawn(move || {
        if let Err(e) = control_loop(
            control,
            conn_for_input,
            root,
            keymap_ctrl,
            ctrl_rx,
            ctrl_send,
            full_tx,
            shutdown_ctrl,
            session_ended_ctrl,
            geo_ctrl,
            (screen_w, screen_h),
            clipboard_ctrl,
            clipboard_rx_ctrl,
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
        // Hidden windows are not part of this attachment: they are announced
        // again when the app maps them (announced starts empty per attachment).
        if !session.is_mapped(win) {
            continue;
        }
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
    // Reattach bookkeeping: everything the session still has has now been
    // announced, so the client can drop windows that disappeared while it was
    // detached. Ordered after the WindowCreate messages above.
    ctrl_tx.send(Message::SyncDone).ok();

    // --- main loop: capture -> bulk channel ------------------------------
    let drop_at = (*drop_after > 0)
        .then(|| Instant::now() + Duration::from_secs(*drop_after));
    let result = serve_loop(
        session,
        bulk,
        &ctrl_tx,
        &full_rx,
        &shutdown,
        geo,
        &ctrl_thread,
        announced,
        app,
        session_end,
        &session_ended,
        drop_at,
    );

    shutdown.store(true, Ordering::SeqCst);
    let _ = ctrl_thread.join();
    result
}

impl Owner {
    /// True once the app has exited (and been reaped).
    fn app_exited(&mut self) -> bool {
        matches!(self.app.as_mut().map(Child::try_wait), Some(Ok(Some(_))))
    }

    /// Stop the app and let the display go: session teardown, not detach.
    fn stop_app(&mut self) {
        if let Some(mut app) = self.app.take() {
            // Already reaped (the session ended because the app exited): do not
            // signal a pid that may have been reused.
            if app.try_wait().ok().flatten().is_some() {
                return;
            }
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
    }
}

/// Serve one client over the legacy `xcomd serve` bootstrap.
pub fn run(args: ServeArgs) -> Result<()> {
    let mut owner = Owner::start(
        &args.display,
        crate::control::DEFAULT_SESSION,
        args.drop_after,
    )?;
    let att = owner.prepare_attachment(&args.net, ssh_client_ip())?;
    if args.json {
        // Never panic on a broken stdout: that is the bootstrap channel.
        let mut out = std::io::stdout().lock();
        writeln!(out, "{}", att.line).context("writing the handshake line")?;
        out.flush().context("flushing the handshake line")?;
    } else {
        logln!("[serve] {}", att.line);
    }
    let never = AtomicBool::new(false);
    let result = serve_attachment(&mut owner, &att, &args.net, &never);
    owner.stop_app();
    result.map(|_| ())
}

/// Run a detached session owner: bind the rendezvous socket, then hand out a
/// fresh handshake and serve each client that asks for one.
pub fn run_session(args: SessionArgs) -> Result<()> {
    crate::control::validate_name(&args.name)?;
    let mut owner = Owner::start(&args.display, &args.name, args.drop_after)?;
    let info = crate::control::Info {
        name: args.name.clone(),
        display: owner.display.clone(),
        app: args.display.exec.clone().unwrap_or_default(),
        pid: std::process::id(),
        created: owner.created,
        idle_timeout: args.idle_timeout,
    };
    let control = crate::control::Control::bind(&args.name, info)?;
    logln!(
        "[session] {} ready (display {}, pid {})",
        args.name,
        owner.display,
        std::process::id()
    );

    let mut idle_since = Instant::now();
    loop {
        if control.kill.load(Ordering::SeqCst) {
            logln!("[session] killed; ending");
            break;
        }
        if owner.app_exited() {
            logln!("[session] app exited; ending");
            break;
        }
        if args.idle_timeout > 0
            && idle_since.elapsed() >= Duration::from_secs(args.idle_timeout)
        {
            logln!(
                "[session] no client for {} s; ending",
                args.idle_timeout
            );
            break;
        }
        match control
            .rx
            .recv_timeout(Duration::from_millis(100))
        {
            Ok(crate::control::Command::Attach { peer, reply }) => {
                let att = match owner.prepare_attachment(&args.net, peer) {
                    Ok(att) => att,
                    Err(e) => {
                        let _ = reply.send(Err(format!("{e:#}")));
                        continue;
                    }
                };
                // The bootstrap only needs the line; if it is already gone, do
                // not bother accepting TLS for nobody.
                if reply.send(Ok(att.line.clone())).is_err() {
                    control.clear_attached();
                    continue;
                }
                match serve_attachment(&mut owner, &att, &args.net, &control.kill) {
                    Ok(AttachmentEnd::AppExited) => {
                        logln!("[session] app exited; ending");
                        break;
                    }
                    Ok(AttachmentEnd::Detached) => {}
                    Err(e) => logln!("[session] attachment ended with error: {e:#}"),
                }
                control.clear_attached();
                idle_since = Instant::now();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    owner.stop_app();
    Ok(())
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
    control: Channel,
    conn: Arc<x11rb::rust_connection::RustConnection>,
    root: Window,
    keymap: Keymap,
    outbound: mpsc::Receiver<Message>,
    outbound_tx: mpsc::Sender<Message>,
    full_requests: mpsc::Sender<Window>,
    shutdown: Arc<AtomicBool>,
    session_ended: Arc<AtomicBool>,
    geo: GeometryMap,
    screen: (u16, u16),
    clipboard: Option<Clipboard>,
    clipboard_rx: ClipboardQueue,
) -> Result<()> {
    let injector = Injector::new(&*conn, root, keymap);
    // XTEST state is global to the display: anything still down when the
    // attachment ends would stay stuck for the next client.
    let mut held_keys: std::collections::HashSet<u32> = std::collections::HashSet::new();
    let mut held_buttons: std::collections::HashSet<u8> = std::collections::HashSet::new();
    let result = control_loop_inner(
        &injector,
        &conn,
        &mut held_keys,
        &mut held_buttons,
        control,
        outbound,
        outbound_tx,
        full_requests,
        shutdown,
        &session_ended,
        geo,
        screen,
        clipboard,
        clipboard_rx,
    );

    // The client is gone: release everything it was holding down.
    for sym in held_keys {
        if let Err(e) = injector.key(sym, false) {
            logln!("[serve] releasing key {sym:#x} failed: {e}");
        }
    }
    for button in held_buttons {
        if let Err(e) = injector.button(button, false) {
            logln!("[serve] releasing button {button} failed: {e}");
        }
    }
    result
}

#[allow(clippy::too_many_arguments)] // loop body, one owner per parameter
fn control_loop_inner(
    injector: &Injector<'_, x11rb::rust_connection::RustConnection>,
    conn: &x11rb::rust_connection::RustConnection,
    held_keys: &mut std::collections::HashSet<u32>,
    held_buttons: &mut std::collections::HashSet<u8>,
    mut control: Channel,
    outbound: mpsc::Receiver<Message>,
    outbound_tx: mpsc::Sender<Message>,
    full_requests: mpsc::Sender<Window>,
    shutdown: Arc<AtomicBool>,
    session_ended: &AtomicBool,
    geo: GeometryMap,
    screen: (u16, u16),
    clipboard: Option<Clipboard>,
    clipboard_rx: ClipboardQueue,
) -> Result<()> {
    let _guard = ShutdownGuard(Arc::clone(&shutdown));
    // Windows already warned about, so a drag past the edge logs once.
    let mut size_warned: std::collections::HashSet<Window> = std::collections::HashSet::new();
    let mut last_rx = Instant::now();
    loop {
        if shutdown.load(Ordering::SeqCst) {
            // The session is ending on purpose (app exit or kill): tell the
            // client before the sockets close, so it exits instead of
            // reconnecting and reporting a "gone" session.
            if session_ended.load(Ordering::SeqCst) {
                let msg = Message::SessionEnded;
                if control.send(msg.msg_type(), &msg.encode()).is_ok() {
                    control.shutdown();
                    logln!("[serve] sent session-ended to the client");
                }
            }
            return Ok(());
        }
        // Window events produced by the capture loop, then clipboard pushes
        // from the owner-owned worker (both feed the control channel).
        while let Ok(msg) = outbound.try_recv() {
            let payload = msg.encode();
            control.send(msg.msg_type(), &payload)?;
        }
        {
            let rx = clipboard_rx.lock().unwrap_or_else(|e| e.into_inner());
            while let Ok(msg) = rx.try_recv() {
                let payload = msg.encode();
                control.send(msg.msg_type(), &payload)?;
            }
        }

        // Poll for inbound input; the short timeout is what lets us service
        // `outbound` promptly. The frame reader keeps partial frames, so this
        // is safe mid-message.
        //
        // A blocking reader thread is not an option: Channel wraps a single
        // rustls stream whose recv() and send() both need &mut, so a reader
        // blocked in recv() would hold the stream and starve writers. The cost
        // here is one recv syscall per 5 ms (0% CPU when idle); if that ever
        // matters, the clean fix is poll(2) on the socket fd plus a self-pipe
        // written by outbound producers, as capture.rs already does for X11.
        control.set_read_timeout(Some(Duration::from_millis(5)))?;
        match control.recv() {
            Ok(Some((ty, payload))) => {
                last_rx = Instant::now();
                match Message::decode(ty, &payload)? {
                Message::InputKey { sym, pressed } => {
                    if xcom_proto::debug_keys() {
                        logln!("[serve] recv key sym={sym:#x} pressed={pressed}");
                    }
                    match injector.key(sym, pressed) {
                        // A single unresolvable keysym must not end the session.
                        Ok(()) if pressed => {
                            held_keys.insert(sym);
                        }
                        Ok(()) => {
                            held_keys.remove(&sym);
                        }
                        Err(e) => logln!("[serve] key {sym:#x} dropped: {e}"),
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
                    if pressed {
                        held_buttons.insert(button);
                    } else {
                        held_buttons.remove(&button);
                    }
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
                    // The screen is the pointer's universe: a window larger
                    // than it has a region no click can reach. Clamp for old
                    // clients; the current client caps itself.
                    let (req_w, req_h) = (w, h);
                    let (w, h) = (req_w.min(screen.0), req_h.min(screen.1));
                    if (w, h) != (req_w, req_h) && size_warned.insert(id) {
                        logln!(
                            "[serve] resize {id:#x} {req_w}x{req_h} exceeds the display {}x{}; \
                             clamped (pass --spawn to enlarge it)",
                            screen.0,
                            screen.1
                        );
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
                    match windows::request_close(conn, id) {
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

/// Interval counters for the client metrics HUD (`Message::Stats`).
#[derive(Default)]
struct Tally {
    frames: u32,
    rects: u32,
    raw: u64,
    comp: u64,
    capture_ns: u64,
    encode_ns: u64,
}

impl Tally {
    fn add(&mut self, st: &FrameStats) {
        self.frames += 1;
        self.rects += st.nrects as u32;
        self.raw += st.raw_bytes as u64;
        self.comp += st.comp_bytes as u64;
        self.capture_ns += st.capture_ns;
        self.encode_ns += st.encode_ns;
    }

    /// Mean per-frame times in microseconds (0 when the interval was empty).
    fn means_us(&self) -> (u32, u32) {
        if self.frames == 0 {
            return (0, 0);
        }
        let n = self.frames as u64;
        ((self.capture_ns / n / 1000) as u32, (self.encode_ns / n / 1000) as u32)
    }
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
    app: &mut Option<Child>,
    session_end: &AtomicBool,
    session_ended: &AtomicBool,
    drop_at: Option<Instant>,
) -> Result<AttachmentEnd> {
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
    let mut tally = Tally::default();
    let mut last_stats = Instant::now();

    loop {
        if shutdown.load(Ordering::SeqCst) {
            return Ok(AttachmentEnd::Detached);
        }
        if session_end.load(Ordering::SeqCst) {
            // `xcomd kill`: the session is ending. Tell the client on the bulk
            // channel *before* dropping it, so the session-ended notice is
            // ordered ahead of the EOF that the reconnect supervisor sees.
            session_ended.store(true, Ordering::SeqCst);
            let msg = Message::SessionEnded;
            let _ = bulk.send(msg.msg_type(), &msg.encode());
            bulk.shutdown();
            return Ok(AttachmentEnd::Detached);
        }
        if drop_at.is_some_and(|at| Instant::now() >= at) {
            logln!("[serve] test aid: dropping the client link");
            shutdown.store(true, Ordering::SeqCst);
            return Ok(AttachmentEnd::Detached);
        }
        // The app defines the session's life: an exit ends it, attached or not.
        if let Some(child) = app.as_mut() {
            match child.try_wait() {
                Ok(Some(_)) => {
                    logln!("[serve] app exited; ending the session");
                    session_ended.store(true, Ordering::SeqCst);
                    let msg = Message::SessionEnded;
                    let _ = bulk.send(msg.msg_type(), &msg.encode());
                    bulk.shutdown();
                    shutdown.store(true, Ordering::SeqCst);
                    return Ok(AttachmentEnd::AppExited);
                }
                Ok(None) => {}
                Err(e) => logln!("[serve] app status: {e}"),
            }
        }
        // The control thread owns input and lifecycle; if it is gone the
        // attachment is over even if the bulk socket still looks writable.
        if ctrl_thread.is_finished() {
            logln!("[serve] control thread exited; detaching");
            shutdown.store(true, Ordering::SeqCst);
            return Ok(AttachmentEnd::Detached);
        }

        // Full-refresh requests (attach, resizes, client-side repair).
        while let Ok(win) = full_rx.try_recv() {
            if let Some((frame, st)) = session.capture_full(win)? {
                tally.add(&st);
                let payload = frame.encode();
                if bulk.send(MSG_DAMAGE, &payload).is_err() {
                    shutdown.store(true, Ordering::SeqCst);
                    return Ok(AttachmentEnd::Detached);
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
            if let Some((frame, st)) = session.capture_full(win)? {
                tally.add(&st);
                let payload = frame.encode();
                if bulk.send(MSG_DAMAGE, &payload).is_err() {
                    shutdown.store(true, Ordering::SeqCst);
                    return Ok(AttachmentEnd::Detached);
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
                    return Ok(AttachmentEnd::Detached);
                }
                stats_frames += 1;
                stats_raw += stats.raw_bytes as u64;
                stats_comp += stats.comp_bytes as u64;
                tally.add(&stats);
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
                        Some((frame, st)) => {
                            tally.add(&st);
                            logln!("[serve] mapped: full frame {win:#x} ({} rects)", frame.rects.len());
                            let payload = frame.encode();
                            if bulk.send(MSG_DAMAGE, &payload).is_err() {
                                shutdown.store(true, Ordering::SeqCst);
                                return Ok(AttachmentEnd::Detached);
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

        // ~1 Hz metrics for the client HUD. Sent even when idle so the client
        // can show capture latency and the server's window count.
        if last_stats.elapsed() >= Duration::from_secs(1) {
            let (capture_us, encode_us) = tally.means_us();
            let msg = Message::Stats {
                interval_ms: last_stats.elapsed().as_millis().min(u32::MAX as u128) as u32,
                frames: tally.frames,
                rects: tally.rects,
                raw_bytes: tally.raw,
                comp_bytes: tally.comp,
                capture_us,
                encode_us,
                windows: session.windows().count() as u16,
            };
            ctrl_tx.send(msg).ok();
            tally = Tally::default();
            last_stats = Instant::now();
        }

        if last_report.elapsed() >= Duration::from_secs(5) {
            last_report = Instant::now();
            // Periodic summary, debug only: the counters are cumulative since
            // session start, so this is noise otherwise.
            if xcom_proto::debug() && stats_frames > 0 {
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
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(win: Window, x: i16, y: i16, w: u16, h: u16) -> GeometryMap {
        let mut m = HashMap::new();
        m.insert(win, (x, y, w, h));
        Arc::new(std::sync::Mutex::new(m))
    }

    #[test]
    fn root_coords_translates_and_clamps() {
        let g = geo(7, 10, 20, 300, 200);
        assert_eq!(root_coords(&g, 7, 5, 7), Some((15, 27)));
        // Out-of-window coordinates clamp to the window edges: XTEST must not
        // land on the desktop outside the remote window.
        assert_eq!(root_coords(&g, 7, -4, -9), Some((10, 20)));
        assert_eq!(root_coords(&g, 7, 299, 199), Some((309, 219)));
        assert_eq!(root_coords(&g, 7, 500, 500), Some((309, 219)));
        assert_eq!(root_coords(&g, 8, 0, 0), None, "untracked window");
        // A zero-size window still resolves to its origin (no negative index).
        let z = geo(9, 5, 6, 0, 0);
        assert_eq!(root_coords(&z, 9, 10, 10), Some((5, 6)));
    }

    #[test]
    fn peer_policy_refuses_unallowlisted_non_loopback() {
        use std::net::{IpAddr, Ipv4Addr};
        let peer: IpAddr = Ipv4Addr::new(192, 0, 2, 1).into();
        let loopback: IpAddr = Ipv4Addr::LOCALHOST.into();
        let any: IpAddr = Ipv4Addr::UNSPECIFIED.into();
        // Over ssh: the peer is allowlisted, whatever the bind address.
        assert!(ensure_peer_policy(Some(peer), false, any).is_ok());
        // Local attach on loopback: no allowlist needed.
        assert!(ensure_peer_policy(None, false, loopback).is_ok());
        // Exposed without a peer or an explicit opt-in: refuse.
        assert!(ensure_peer_policy(None, false, any).is_err());
        // --allow-any-peer is the documented escape hatch.
        assert!(ensure_peer_policy(None, true, any).is_ok());
    }
}
