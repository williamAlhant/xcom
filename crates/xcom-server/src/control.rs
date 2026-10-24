//! Detached-session rendezvous over a Unix socket.
//!
//! A `xcomd session` owner listens at `$XDG_RUNTIME_DIR/xcomd/<name>.sock`.
//! `xcomd attach` (the ssh bootstrap) asks it for a handshake, and `xcomd
//! sessions` / `xcomd kill` inspect or end sessions. The socket carries one
//! JSON line per request; an attach answers with the same handshake line
//! `xcomd serve` used to print.
//!
//! The socket is per user: the directory is 0700 and the socket 0600, so a
//! local attacker cannot ask for a handshake (and thus cannot learn the
//! single-use channel tokens).

use std::io::{BufRead, BufReader, Write};
use std::net::IpAddr;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use xcom_proto::logln;

pub const DEFAULT_SESSION: &str = "default";

/// Where session sockets and logs live for this user.
fn socket_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if Path::new(&dir).is_absolute() => PathBuf::from(dir),
        _ => std::env::temp_dir().join(format!("xcomd-{}", unsafe { libc::getuid() })),
    };
    let dir = base.join("xcomd");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("securing {}", dir.display()))?;
    Ok(dir)
}

/// Socket for one session; rejects names that could escape the directory.
pub fn socket_path(name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(socket_dir()?.join(format!("{name}.sock")))
}

fn log_path(name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(socket_dir()?.join(format!("{name}.log")))
}

/// Session names end up in a file name, a socket, and logs: keep them boring.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 64 {
        bail!("session name must be 1-64 characters");
    }
    if name == "." || name == ".." {
        bail!("invalid session name {name:?}");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        bail!("session name may only contain letters, digits, '-', '_' and '.'");
    }
    Ok(())
}

/// What `sessions` and an attach request see about a running owner.
#[derive(Clone)]
pub struct Info {
    pub name: String,
    pub display: String,
    pub app: String,
    pub pid: u32,
    pub created: u64,
    pub idle_timeout: u64,
}

impl Info {
    fn line(&self, attached: bool) -> String {
        format!(
            "{{\"ok\":true,\"name\":\"{}\",\"display\":\"{}\",\"app\":\"{}\",\"pid\":{},\
             \"created\":{},\"idle_timeout\":{},\"attached\":{}}}",
            json_escape(&self.name),
            json_escape(&self.display),
            json_escape(&self.app),
            self.pid,
            self.created,
            self.idle_timeout,
            attached
        )
    }
}

/// An attach request handed from the socket thread to the owner loop.
pub enum Command {
    Attach {
        peer: Option<IpAddr>,
        /// The owner replies with the handshake line, or a reason it failed.
        reply: mpsc::SyncSender<Result<String, String>>,
    },
}

/// The owner's control socket. Dropping it removes the socket file.
pub struct Control {
    pub rx: mpsc::Receiver<Command>,
    /// Set by `xcomd kill`; the owner and the attachment loop poll it.
    pub kill: Arc<AtomicBool>,
    attached: Arc<AtomicBool>,
    path: PathBuf,
}

impl Control {
    pub fn bind(name: &str, info: Info) -> Result<Control> {
        let path = socket_path(name)?;
        // A socket that still answers means the name is taken. A stale file
        // (owner crashed or was SIGKILLed) is removed.
        if path.exists() {
            match UnixStream::connect(&path) {
                Ok(_) => bail!("session {name} is already running"),
                Err(_) => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("binding {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing {}", path.display()))?;
        listener
            .set_nonblocking(true)
            .context("configuring the session socket")?;

        let (tx, rx) = mpsc::channel();
        let attached = Arc::new(AtomicBool::new(false));
        let kill = Arc::new(AtomicBool::new(false));
        let info = Arc::new(info);
        let attached_thread = Arc::clone(&attached);
        let kill_thread = Arc::clone(&kill);
        std::thread::spawn(move || {
            serve_socket(listener, info, tx, attached_thread, kill_thread)
        });

        Ok(Control {
            rx,
            kill,
            attached,
            path,
        })
    }

    /// The attachment is over (detached, failed to start, or the client left).
    pub fn clear_attached(&self) {
        self.attached.store(false, Ordering::SeqCst);
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn serve_socket(
    listener: UnixListener,
    info: Arc<Info>,
    commands: mpsc::Sender<Command>,
    attached: Arc<AtomicBool>,
    kill: Arc<AtomicBool>,
) {
    while !kill.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => handle(stream, &info, &commands, &attached, &kill),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                logln!("[control] accept: {e}");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn handle(
    mut stream: UnixStream,
    info: &Info,
    commands: &mpsc::Sender<Command>,
    attached: &AtomicBool,
    kill: &AtomicBool,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut reader = match stream.try_clone() {
        Ok(clone) => BufReader::new(clone),
        Err(_) => return,
    };
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    match field(&line, "cmd").as_deref() {
        Some("attach") => {
            // Reserve the slot before handing the request to the owner loop, so
            // two bootstraps cannot both think they got it.
            if attached.swap(true, Ordering::SeqCst) {
                reply(&mut stream, "{\"ok\":false,\"error\":\"session busy\"}");
                return;
            }
            let peer = field(&line, "peer").and_then(|p| p.parse().ok());
            let (tx, rx) = mpsc::sync_channel(1);
            if commands.send(Command::Attach { peer, reply: tx }).is_err() {
                attached.store(false, Ordering::SeqCst);
                reply(
                    &mut stream,
                    "{\"ok\":false,\"error\":\"session is shutting down\"}",
                );
                return;
            }
            match rx.recv_timeout(Duration::from_secs(20)) {
                Ok(Ok(handshake)) => {
                    let _ = writeln!(stream, "{handshake}");
                    let _ = stream.flush();
                }
                Ok(Err(e)) => {
                    attached.store(false, Ordering::SeqCst);
                    reply(
                        &mut stream,
                        &format!(
                            "{{\"ok\":false,\"error\":\"{}\"}}",
                            json_escape(&e)
                        ),
                    );
                }
                Err(_) => {
                    attached.store(false, Ordering::SeqCst);
                    reply(
                        &mut stream,
                        "{\"ok\":false,\"error\":\"attach timed out\"}",
                    );
                }
            }
        }
        Some("info") => {
            let _ = writeln!(stream, "{}", info.line(attached.load(Ordering::SeqCst)));
        }
        Some("kill") => {
            kill.store(true, Ordering::SeqCst);
            reply(&mut stream, "{\"ok\":true}");
        }
        Some(other) => reply(
            &mut stream,
            &format!(
                "{{\"ok\":false,\"error\":\"unknown command {}\"}}",
                json_escape(other)
            ),
        ),
        None => reply(&mut stream, "{\"ok\":false,\"error\":\"bad request\"}"),
    }
}

fn reply(stream: &mut UnixStream, line: &str) {
    let _ = writeln!(stream, "{line}");
    let _ = stream.flush();
}

/// ssh bootstrap: attach to (or create) a session and print its handshake.
#[derive(clap::Args)]
pub struct AttachArgs {
    /// Session name.
    #[arg(long, default_value = DEFAULT_SESSION)]
    pub name: String,
    /// Create the session if it does not exist.
    #[arg(long)]
    pub create: bool,
    /// Tear an existing session down and start a fresh one (destructive).
    #[arg(long)]
    pub replace: bool,
    /// Print the handshake line as JSON on stdout (the client bootstrap).
    #[arg(long)]
    pub json: bool,
    /// Idle timeout for a newly created session (0 = never).
    #[arg(long, default_value_t = 0)]
    pub idle_timeout: u64,
    /// Test aid: drop each client link after N seconds (0 = never).
    #[arg(long, default_value_t = 0)]
    pub drop_after: u64,
    #[command(flatten)]
    pub display: crate::serve::DisplayArgs,
    #[command(flatten)]
    pub net: crate::serve::NetArgs,
}

pub fn attach(args: &AttachArgs) -> Result<()> {
    validate_name(&args.name)?;
    let path = socket_path(&args.name)?;
    let create = args.create || args.replace;

    if args.replace {
        if let Ok(mut stream) = UnixStream::connect(&path) {
            let _ = writeln!(stream, "{{\"cmd\":\"kill\"}}");
            let _ = stream.flush();
            wait_until_gone(&path, Duration::from_secs(10));
        }
    }

    let mut spawned = false;
    let mut stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(_) if create => {
            spawn_session(args)?;
            spawned = true;
            connect_wait(&path, &args.name, Duration::from_secs(20))?
        }
        Err(e) => bail!(
            "session {} is gone ({e}); use --create to start it",
            args.name
        ),
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(25)))
        .context("configuring the session socket")?;

    // The owner must allowlist the client's TLS address, which is this ssh
    // process's peer address -- not the detached owner's inherited
    // SSH_CONNECTION (that would pin the first laptop's IP forever).
    let peer = crate::serve::ssh_client_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_default();
    writeln!(stream, "{{\"cmd\":\"attach\",\"peer\":\"{peer}\"}}")
        .context("asking the session for a handshake")?;
    stream.flush().context("flushing the attach request")?;

    let mut line = String::new();
    let read = BufReader::new(stream).read_line(&mut line);
    if let Err(e) = read {
        if spawned {
            kill_quiet(&args.name);
        }
        return Err(e).context("reading the handshake from the session");
    }
    if let Some(error) = field(&line, "error") {
        // Do not leave an owner we just started for a request it refused
        // (e.g. a peer-less --listen on a public address).
        if spawned {
            kill_quiet(&args.name);
        }
        bail!("{error}");
    }
    if !line.contains("\"ok\":true") {
        if spawned {
            kill_quiet(&args.name);
        }
        bail!(
            "session {} did not return a handshake: {}",
            args.name,
            line.trim_end()
        );
    }
    if args.json {
        // Never panic on a broken stdout: that is the bootstrap channel.
        let mut out = std::io::stdout().lock();
        writeln!(out, "{}", line.trim_end()).context("writing the handshake line")?;
        out.flush().context("flushing the handshake line")?;
    } else {
        logln!("[attach] {}", line.trim_end());
    }
    Ok(())
}

/// Best-effort cleanup of an owner this bootstrap just spawned: a refused
/// first attach must not leave an idle session behind.
fn kill_quiet(name: &str) {
    let Ok(path) = socket_path(name) else {
        return;
    };
    if let Ok(mut stream) = UnixStream::connect(&path) {
        let _ = writeln!(stream, "{{\"cmd\":\"kill\"}}");
        let _ = stream.flush();
    }
}

/// Start a detached owner. It outlives this bootstrap process (and ssh).
fn spawn_session(args: &AttachArgs) -> Result<()> {
    let exe = std::env::current_exe().context("locating xcomd")?;
    let log = log_path(&args.name)?;
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .with_context(|| format!("opening {}", log.display()))?;
    let err = out.try_clone().context("duplicating the session log")?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("session")
        .arg("--name")
        .arg(&args.name)
        .arg("--spawn")
        .arg(&args.display.spawn)
        .arg("--idle-timeout")
        .arg(args.idle_timeout.to_string());
    if args.drop_after > 0 {
        cmd.arg("--drop-after").arg(args.drop_after.to_string());
    }
    if let Some(display) = &args.display.display {
        cmd.arg("--display").arg(display);
    }
    if let Some(exec) = &args.display.exec {
        cmd.arg("--exec").arg(exec);
    }
    if args.net.allow_any_peer {
        cmd.arg("--allow-any-peer");
    }
    if let Some(connect) = &args.net.connect {
        cmd.arg("--connect").arg(connect);
    }
    if args.net.listen != "auto" {
        cmd.arg("--listen").arg(&args.net.listen);
    }
    if args.net.accept_timeout != 120 {
        cmd.arg("--accept-timeout")
            .arg(args.net.accept_timeout.to_string());
    }

    // New session: sshd closing its channel (or the bootstrap exiting) must not
    // SIGHUP the owner. Xvfb and the app still die with the owner via
    // PR_SET_PDEATHSIG in serve.rs.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.stdin(Stdio::null()).stdout(out).stderr(err);
    let child = cmd.spawn().context("starting the session owner")?;
    logln!(
        "[attach] starting session {} (pid {}, log {})",
        args.name,
        child.id(),
        log.display()
    );
    // Deliberately not waiting: the owner is meant to outlive us.
    drop(child);
    Ok(())
}

fn connect_wait(path: &Path, name: &str, timeout: Duration) -> Result<UnixStream> {
    let deadline = Instant::now() + timeout;
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(e) if Instant::now() >= deadline => {
                let log = log_path(name)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                bail!("session {name} did not start ({e}); see {log}");
            }
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn wait_until_gone(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if UnixStream::connect(path).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `xcomd sessions`: list the sessions running for this user.
pub fn list() -> Result<()> {
    let dir = socket_dir()?;
    let mut found = 0;
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => {
            println!("no sessions");
            return Ok(());
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("sock") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(line) = query(&path) else {
            continue; // stale socket: the owner is gone
        };
        let display = field(&line, "display").unwrap_or_default();
        let app = field(&line, "app").unwrap_or_default();
        let pid = number(&line, "pid").unwrap_or(0);
        let created = number(&line, "created").unwrap_or(0);
        let idle = number(&line, "idle_timeout").unwrap_or(0);
        let attached = boolean(&line, "attached");
        let age = now_unix().saturating_sub(created);
        println!(
            "{name}\tdisplay {display}\tpid {pid}\tup {}\t{}\tidle {}\tapp: {}",
            human_age(age),
            if attached { "attached" } else { "detached" },
            if idle == 0 {
                "never".to_string()
            } else {
                format!("{idle}s")
            },
            if app.is_empty() { "-" } else { &app },
        );
        found += 1;
    }
    if found == 0 {
        println!("no sessions");
    }
    Ok(())
}

fn query(path: &Path) -> Result<String> {
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    writeln!(stream, "{{\"cmd\":\"info\"}}")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(line)
}

/// `xcomd kill`: end a session (the owner kills the app and removes Xvfb).
#[derive(clap::Args)]
pub struct KillArgs {
    /// Session name.
    #[arg(long, default_value = DEFAULT_SESSION)]
    pub session: String,
}

pub fn kill(name: &str) -> Result<()> {
    let path = socket_path(name)?;
    let mut stream = UnixStream::connect(&path)
        .with_context(|| format!("session {name} is not running"))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    writeln!(stream, "{{\"cmd\":\"kill\"}}")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    if line.contains("\"ok\":true") {
        logln!("[kill] session {name} is stopping");
        Ok(())
    } else {
        bail!(
            "{}",
            field(&line, "error").unwrap_or_else(|| "kill failed".to_string())
        )
    }
}

/// Minimal JSON field readers: the protocol is line-JSON and all values here
/// are strings, numbers or booleans (no serde dependency).
fn field(line: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\":\"");
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn number(line: &str, key: &str) -> Option<u64> {
    let pat = format!("\"{key}\":");
    let start = line.find(&pat)? + pat.len();
    let rest = &line[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn boolean(line: &str, key: &str) -> bool {
    let pat = format!("\"{key}\":");
    line.find(&pat)
        .map(|i| line[i + pat.len()..].starts_with("true"))
        .unwrap_or(false)
}

pub(crate) fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn human_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}
