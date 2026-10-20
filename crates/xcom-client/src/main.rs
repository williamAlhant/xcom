//! xcom client: connect to a remote session and render its X windows as
//! native windows.
//!
//! The ssh bootstrap only exists to authenticate: `ssh user@host
//! 'xcomd serve --json'` returns one handshake line (channel address, token,
//! pinned certificate fingerprint), after which all traffic flows over the
//! dedicated TLS channel opened here.

mod keys;
mod ui;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use winit::event_loop::EventLoop;

use xcom_proto::{Message, MSG_DAMAGE};
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
    /// Session token, hex (with --direct).
    #[arg(long)]
    token: Option<String>,
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
}

struct Handshake {
    addr: String,
    token: [u8; 32],
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

fn token32(bytes: Vec<u8>) -> Result<[u8; 32]> {
    bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("token must be exactly 32 bytes"))
}

fn hex_decode(s: &str) -> Result<Vec<u8>> {
    if s.len() % 2 != 0 {
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
            let token = hex_decode(&json_field(&line, "token").context("handshake line: token")?)?;
            let fingerprint =
                json_field(&line, "cert_sha256").context("handshake line: cert_sha256")?;
            let token = token32(token)?;
            return Ok(Handshake {
                addr,
                token,
                fingerprint,
                ssh: Some(child),
            });
        }
        // Anything else is daemon log output; show it.
        eprintln!("[remote] {line}");
    }
}

fn bootstrap_direct(addr: &str, token: &str, fingerprint: &str) -> Result<Handshake> {
    let token = hex_decode(token)?;
    Ok(Handshake {
        addr: addr.to_string(),
        token: token32(token)?,
        fingerprint: fingerprint.to_string(),
        ssh: None,
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let Cmd::Connect(args) = cli.cmd;

    let mut handshake = match (&args.ssh, &args.direct) {
        (Some(target), _) => {
            bootstrap_ssh(target, args.ssh_args.as_deref(), args.exec.as_deref())?
        }
        (None, Some(addr)) => bootstrap_direct(
            addr,
            args.token.as_deref().context("--direct needs --token")?,
            args.fingerprint
                .as_deref()
                .context("--direct needs --fingerprint")?,
        )?,
        (None, None) => bail!("either --ssh user@host or --direct addr is required"),
    };
    if let Some(override_addr) = &args.connect {
        handshake.addr = override_addr.clone();
    }

    eprintln!(
        "[xcom] connecting to {} (fingerprint {}…)",
        handshake.addr,
        &handshake.fingerprint[..16.min(handshake.fingerprint.len())]
    );

    let timeout = Duration::from_secs(15);
    let control = xcom_transport::connect(
        &handshake.addr,
        Role::Control,
        &handshake.fingerprint,
        &handshake.token,
        timeout,
    )
    .context("control channel")?;
    let bulk = xcom_transport::connect(
        &handshake.addr,
        Role::Bulk,
        &handshake.fingerprint,
        &handshake.token,
        timeout,
    )
    .context("bulk channel")?;
    eprintln!("[xcom] secure channel established");

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("creating the event loop (a display server is required for the client)")?;
    let proxy = event_loop.create_proxy();
    let (input_tx, input_rx) = mpsc::channel::<Message>();

    // Control thread: input out, window lifecycle in.
    let proxy_ctrl = proxy.clone();
    std::thread::spawn(move || {
        if let Err(e) = control_loop(control, proxy_ctrl.clone(), input_rx) {
            eprintln!("[xcom] control channel: {e}");
        }
        // EOF or error: the session is gone — make the UI close its windows
        // instead of leaving stale content on screen.
        proxy_ctrl.send_event(UserEvent::ConnectionLost).ok();
    });

    if let Some(text) = args.type_text.clone() {
        let tx = input_tx.clone();
        let delay = args.type_delay;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(delay));
            eprintln!("[xcom] typing {text:?} t_ns={}", xcom_proto::now_ns());
            for c in text.chars() {
                // Control characters map to their X keysyms, not their codes.
                let sym = match c {
                    '\u{8}' => 0xff08, // BackSpace
                    '\n' => 0xff0d,    // Return
                    '\t' => 0xff09,    // Tab
                    _ => c as u32,
                };
                for pressed in [true, false] {
                    tx.send(Message::InputKey { sym, pressed }).ok();
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
            eprintln!("[xcom] bulk channel: {e}");
        }
        proxy_bulk.send_event(UserEvent::ConnectionLost).ok();
    });

    let mut app = App::new(proxy, input_tx, args.dump_ppm.clone(), args.fullscreen);
    let result = event_loop.run_app(&mut app).context("event loop");

    if let Some(mut ssh) = handshake.ssh.take() {
        let _ = ssh.kill();
        let _ = ssh.wait();
    }
    result
}

fn control_loop(
    mut control: Channel,
    proxy: winit::event_loop::EventLoopProxy<UserEvent>,
    input_rx: mpsc::Receiver<Message>,
) -> Result<()> {
    loop {
        // Input generated by the UI thread.
        while let Ok(msg) = input_rx.try_recv() {
            let bye = msg == Message::Bye;
            let payload = msg.encode();
            if control.send(msg.msg_type(), &payload).is_err() {
                return Ok(()); // server is gone
            }
            if bye {
                control.shutdown();
                return Ok(());
            }
        }

        control.set_read_timeout(Some(Duration::from_millis(5)))?;
        match control.recv() {
            Ok(Some((ty, payload))) => {
                let Ok(msg) = Message::decode(ty, &payload) else {
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
                    } => Some(UserEvent::Open {
                        id,
                        x,
                        y,
                        w,
                        h,
                        title,
                    }),
                    Message::WindowGeometry { id, w, h } => Some(UserEvent::Geometry { id, w, h }),
                    Message::WindowDestroy { id } => Some(UserEvent::Close { id }),
                    _ => None,
                };
                if let Some(ev) = event {
                    proxy.send_event(ev).ok();
                }
            }
            Ok(None) => return Ok(()), // EOF
            Err(e) if is_timeout(&e) => continue,
            Err(e) => return Err(e.into()),
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
            Ok(Some((ty, _))) => eprintln!("[xcom] bulk: unexpected msg type {ty}"),
            Ok(None) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
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
