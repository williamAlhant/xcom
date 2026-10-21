//! M0 spike harness: measures the capture->compress->wire->decode pipeline
//! and the XTEST input path against a real X server.
//!
//! Subcommands:
//!   `m0 anim`        animated test window (damage source + input log)
//!   `m0 capture`     capture a window, LZ4 it, stream frames to a sink
//!   `m0 sink`        receive frames, report latency/throughput percentiles
//!   `m0 input-test`  inject keysyms, verify and time what the app receives

use std::io::{BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Subcommand, ValueEnum};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, Gcontext,
    ImageFormat, PropMode, Rectangle, Window, WindowClass,
};
use x11rb::protocol::Event;

use xcom_codec::decompress;
use xcom_proto::{now_ns, DamageFrame, FrameReader, FrameWriter, MSG_DAMAGE, MSG_HELLO, PROTO_VERSION};
use xcom_x11::capture::{CaptureConfig, CaptureSession, DamageMode, PumpEvent};
use xcom_x11::input::Injector;
use xcom_x11::keymap::{Keymap, XK_ESCAPE, XK_RETURN};
use xcom_x11::windows;

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Animated test window (damage source, logs injected input).
    Anim(AnimArgs),
    /// Capture a window and stream compressed frames to a sink.
    Capture(CaptureArgs),
    /// Receive frames and measure end-to-end latency and throughput.
    Sink(SinkArgs),
    /// Inject keystrokes and verify+time what the app actually receives.
    InputTest(InputTestArgs),
    /// Draw/erase a small cell through the real capture path and check pixels.
    Selftest(SelftestArgs),
}

pub fn run(args: Args) -> Result<()> {
    match args.cmd {
        Cmd::Anim(a) => anim(a),
        Cmd::Capture(a) => capture(a),
        Cmd::Sink(a) => sink(a),
        Cmd::InputTest(a) => input_test(a),
        Cmd::Selftest(a) => selftest(a),
    }
}

fn parse_size(s: &str) -> Result<(u16, u16)> {
    let (w, h) = s
        .split_once('x')
        .with_context(|| format!("size must look like 800x600, got {s:?}"))?;
    Ok((w.parse()?, h.parse()?))
}

fn parse_window(s: &str) -> Option<Window> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    u32::from_str_radix(s, 16).ok()
}

// ---------------------------------------------------------------------------
// anim
// ---------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct AnimArgs {
    /// X display; defaults to $DISPLAY, then :9.
    #[arg(long, default_value = "")]
    pub display: String,
    /// Window size, e.g. 800x600.
    #[arg(long, default_value = "800x600")]
    pub size: String,
    #[arg(long, default_value_t = 60)]
    pub fps: u64,
    /// Damage pattern: a small moving box, or a full-window repaint.
    #[arg(long, value_enum, default_value = "box")]
    pub mode: AnimMode,
    /// Exit after N seconds (0 = run until Escape).
    #[arg(long, default_value_t = 0)]
    pub seconds: u64,
    /// File to log received input events to.
    #[arg(long)]
    pub input_log: Option<String>,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum AnimMode {
    Box,
    Full,
}

fn anim(args: AnimArgs) -> Result<()> {
    let (w, h) = parse_size(&args.size)?;
    let display = if args.display.is_empty() {
        std::env::var("DISPLAY").unwrap_or_else(|_| ":9".to_string())
    } else {
        args.display.clone()
    };
    let (conn, screen_num) = x11rb::connect(Some(display.as_str()))
        .with_context(|| format!("connecting to {display}"))?;
    let screen = &conn.setup().roots[screen_num];
    let root = screen.root;

    let win: Window = conn.generate_id()?;
    conn.create_window(
        0, // depth = CopyFromParent
        win,
        root,
        0,
        0,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(screen.black_pixel)
            .event_mask(
                EventMask::KEY_PRESS
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::EXPOSURE,
            ),
    )?;
    conn.change_property(
        PropMode::REPLACE,
        win,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        8u8,
        12u32,
        b"xcom-m0-anim",
    )?;
    conn.map_window(win)?;

    let gc_bg: Gcontext = conn.generate_id()?;
    conn.create_gc(
        gc_bg,
        win,
        &CreateGCAux::new()
            .foreground(screen.black_pixel)
            .graphics_exposures(0u32),
    )?;
    let gc_fg: Gcontext = conn.generate_id()?;
    conn.create_gc(
        gc_fg,
        win,
        &CreateGCAux::new()
            .foreground(screen.white_pixel)
            .graphics_exposures(0u32),
    )?;
    conn.flush()?;

    println!("WINDOW {win:#x}");

    let keymap = Keymap::query(&conn)?;
    let mut log = match &args.input_log {
        Some(path) => Some(std::fs::File::create(path)?),
        None => None,
    };

    let period = Duration::from_nanos(1_000_000_000 / args.fps.max(1));
    let deadline = (args.seconds > 0).then(|| Instant::now() + Duration::from_secs(args.seconds));
    let mut next = Instant::now();
    let mut frame: u64 = 0;
    let mut quit = false;
    let mut px: i16 = 0;
    let mut py: i16 = 0;
    let mut redraw = true;
    let box_w = 120u16;
    let box_h = 120u16;
    let mut size = (w, h);

    while !quit {
        if redraw {
            // Like a real app: repaint everything on expose/resize.
            conn.poly_fill_rectangle(
                win,
                gc_bg,
                &[Rectangle {
                    x: 0,
                    y: 0,
                    width: size.0,
                    height: size.1,
                }],
            )?;
            redraw = false;
        }
        match args.mode {
            AnimMode::Box => {
                conn.poly_fill_rectangle(
                    win,
                    gc_bg,
                    &[Rectangle {
                        x: px,
                        y: py,
                        width: box_w,
                        height: box_h,
                    }],
                )?;
                let (w, h) = size;
                px = ((frame as i16 * 13).rem_euclid(w.saturating_sub(box_w).max(1) as i16)).abs();
                py = ((frame as i16 * 7).rem_euclid(h.saturating_sub(box_h).max(1) as i16)).abs();
                conn.poly_fill_rectangle(
                    win,
                    gc_fg,
                    &[Rectangle {
                        x: px,
                        y: py,
                        width: box_w,
                        height: box_h,
                    }],
                )?;
            }
            AnimMode::Full => {
                let (w, h) = size;
                let mut buf = vec![0u8; w as usize * h as usize * 4];
                let t = (frame * 5 % 256) as u8;
                for y in 0..h as usize {
                    for x in 0..w as usize {
                        let o = (y * w as usize + x) * 4;
                        buf[o] = (x * 255 / w as usize) as u8; // B
                        buf[o + 1] = (y * 255 / h as usize) as u8; // G
                        buf[o + 2] = t; // R
                        buf[o + 3] = 0;
                    }
                }
                conn.put_image(ImageFormat::Z_PIXMAP, win, gc_fg, w, h, 0, 0, 0, 24, &buf)?;
            }
        }
        conn.flush()?;
        frame += 1;

        next += period;
        // Wait for the next frame in small quanta, polling events in between:
        // a real app's event loop is not bound by its frame period, and neither
        // should this measurement be.
        while Instant::now() < next {
            while let Some(ev) = conn.poll_for_event()? {
                handle_anim_event(ev, &keymap, &mut log, &mut quit, win, &mut size, &mut redraw)?;
            }
            std::thread::sleep(Duration::from_micros(200));
        }

        while let Some(ev) = conn.poll_for_event()? {
            handle_anim_event(ev, &keymap, &mut log, &mut quit, win, &mut size, &mut redraw)?;
        }

        if let Some(d) = deadline {
            if Instant::now() >= d {
                quit = true;
            }
        }
    }
    Ok(())
}

fn handle_anim_event(
    ev: Event,
    keymap: &Keymap,
    log: &mut Option<std::fs::File>,
    quit: &mut bool,
    win: Window,
    size: &mut (u16, u16),
    redraw: &mut bool,
) -> Result<()> {
    match ev {
        Event::KeyPress(e) => {
            let mods = u16::from(e.state);
            let sym = keymap.keysym_for(e.detail, mods).unwrap_or(0);
            if sym == XK_ESCAPE {
                *quit = true;
            }
            if let Some(log) = log.as_mut() {
                writeln!(
                    log,
                    "recv sym={sym:#x} code={} state={mods:#x} t_ns={}",
                    e.detail,
                    now_ns()
                )?;
                log.flush()?;
            }
        }
        Event::ConfigureNotify(e) if e.window == win => {
            println!("RESIZED {}x{}", e.width, e.height);
            size.0 = e.width;
            size.1 = e.height;
            *redraw = true;
        }
        Event::Expose(_) => *redraw = true,
        _ => {}
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// capture
// ---------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct CaptureArgs {
    #[arg(long, default_value = ":9")]
    pub display: String,
    /// Window id in hex (0x1234) or auto:<name-substr> / auto:first.
    #[arg(long)]
    pub window: String,
    #[arg(long, default_value = "127.0.0.1:7777")]
    pub target: String,
    /// Run for N seconds (0 = until the sink disconnects).
    #[arg(long, default_value_t = 10)]
    pub seconds: u64,
    #[arg(long, value_enum, default_value = "nonempty")]
    pub damage: DamageArg,
    #[arg(long, default_value_t = 32)]
    pub max_rects: usize,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
pub enum DamageArg {
    Raw,
    NonEmpty,
}

fn resolve_window(conn: &impl Connection, root: Window, spec: &str) -> Result<Window> {
    if let Some(win) = parse_window(spec) {
        return Ok(win);
    }
    let needle = spec.strip_prefix("auto:").unwrap_or("first");
    let tops = windows::list_top_levels(conn, root, true)?;
    let candidates: Vec<String> = tops
        .iter()
        .map(|t| {
            format!(
                "{:#x}{} {}x{} {:?}",
                t.window,
                if t.mapped { "" } else { " (unmapped)" },
                t.w,
                t.h,
                t.name
            )
        })
        .collect();
    let pick = if needle == "first" {
        tops.into_iter().find(|t| t.mapped)
    } else {
        tops.into_iter()
            .find(|t| t.mapped && t.name.to_lowercase().contains(&needle.to_lowercase()))
    };
    pick.map(|t| t.window).with_context(|| {
        format!(
            "no mapped top-level matching {spec:?}; top-levels on this display: [{}]",
            candidates.join(", ")
        )
    })
}

fn capture(args: CaptureArgs) -> Result<()> {
    let (conn, screen_num) = x11rb::connect(Some(&args.display))
        .with_context(|| format!("connecting to {}", args.display))?;
    let root = conn.setup().roots[screen_num].root;
    let win = resolve_window(&conn, root, &args.window)?;

    let cfg = CaptureConfig {
        mode: match args.damage {
            DamageArg::Raw => DamageMode::Raw,
            DamageArg::NonEmpty => DamageMode::NonEmpty,
        },
        max_rects: args.max_rects,
    };
    let mut cap = CaptureSession::from_connection(conn, screen_num, cfg)?;
    let (_, _, win_w, win_h) = cap.add_window(win)?;

    let stream = TcpStream::connect(&args.target)
        .with_context(|| format!("connecting to sink {}", args.target))?;
    stream.set_nodelay(true)?;
    let mut w = FrameWriter::new(stream);

    let mut hello = Vec::new();
    hello.extend_from_slice(&PROTO_VERSION.to_le_bytes());
    hello.extend_from_slice(&win.to_le_bytes());
    w.write(MSG_HELLO, &hello)?;

    eprintln!("capturing {win:#010x} ({win_w}x{win_h}) -> {}", args.target);

    let deadline =
        (args.seconds > 0).then(|| Instant::now() + Duration::from_secs(args.seconds));
    let mut frames = 0u64;
    let mut raw_total = 0u64;
    let mut comp_total = 0u64;
    let mut rect_total = 0u64;
    let mut capture_ns_total = 0u64;
    let mut encode_ns_total = 0u64;
    let mut interval = Instant::now();
    let start = Instant::now();

    loop {
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break;
            }
        }
        let Some(PumpEvent::Frame { frame, stats, .. }) = cap.pump(deadline)? else {
            continue;
        };
        let payload = frame.encode();
        if w.write(MSG_DAMAGE, &payload).is_err() {
            eprintln!("sink closed the connection");
            break;
        }

        frames += 1;
        raw_total += stats.raw_bytes as u64;
        comp_total += stats.comp_bytes as u64;
        rect_total += stats.nrects as u64;
        capture_ns_total += stats.capture_ns;
        encode_ns_total += stats.encode_ns;

        if interval.elapsed() >= Duration::from_secs(1) {
            let secs = interval.elapsed().as_secs_f64();
            let n = frames.max(1);
            eprintln!(
                "[cap] fps={:.1} frames={frames} rects={} raw={:.1}MB comp={:.1}MB ratio={:.1}x capture_avg={:.2}ms encode_avg={:.2}ms out={:.2}MB/s",
                frames as f64 / start.elapsed().as_secs_f64(),
                rect_total,
                raw_total as f64 / 1e6,
                comp_total as f64 / 1e6,
                if comp_total > 0 { raw_total as f64 / comp_total as f64 } else { 0.0 },
                capture_ns_total as f64 / n as f64 / 1e6,
                encode_ns_total as f64 / n as f64 / 1e6,
                comp_total as f64 / 1e6 / secs,
            );
            interval = Instant::now();
        }
    }

    let secs = start.elapsed().as_secs_f64().max(1e-6);
    let n = frames.max(1);
    println!(
        "SUMMARY frames={frames} rects={rect_total} raw_mb={:.2} comp_mb={:.2} ratio={:.2}x fps={:.1} capture_avg_ms={:.3} encode_avg_ms={:.3} wire_mbps={:.2}",
        raw_total as f64 / 1e6,
        comp_total as f64 / 1e6,
        if comp_total > 0 { raw_total as f64 / comp_total as f64 } else { 0.0 },
        frames as f64 / secs,
        capture_ns_total as f64 / n as f64 / 1e6,
        encode_ns_total as f64 / n as f64 / 1e6,
        comp_total as f64 * 8.0 / 1e6 / secs,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// sink
// ---------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct SinkArgs {
    #[arg(long, default_value = "127.0.0.1:7777")]
    pub listen: String,
    /// Stop after N seconds (0 = until EOF).
    #[arg(long, default_value_t = 0)]
    pub seconds: u64,
    /// Also decompress every rect (measures the client-side decode cost).
    #[arg(long, default_value_t = true)]
    pub decode: bool,
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

/// Keysyms of modifier keys (Shift/Control/Alt/Mod*, ISO level 3, Mode_switch).
fn is_modifier_sym(sym: u32) -> bool {
    matches!(sym, 0xffe1..=0xffee | 0xfe03 | 0xff7e)
}

fn sink(args: SinkArgs) -> Result<()> {
    let listener = TcpListener::bind(&args.listen)
        .with_context(|| format!("binding {}", args.listen))?;
    eprintln!("[sink] listening on {}", args.listen);

    // Bound the wait for a connection too: --seconds is a wall-clock limit for
    // the whole run, not just for the connected phase.
    let accept_deadline =
        (args.seconds > 0).then(|| Instant::now() + Duration::from_secs(args.seconds));
    listener.set_nonblocking(true)?;
    let sock = loop {
        match listener.accept() {
            Ok((sock, peer)) => {
                eprintln!("[sink] connection from {peer}");
                break sock;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if accept_deadline.is_some_and(|d| Instant::now() >= d) {
                    bail!("no connection within {}s", args.seconds);
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(e.into()),
        }
    };
    sock.set_nonblocking(false)?;
    sock.set_nodelay(true)?;

    // A watchdog shuts the socket down at the deadline: the frame reader below
    // blocks in read_exact and would otherwise never notice the deadline.
    if args.seconds > 0 {
        let guard = sock.try_clone()?;
        let secs = args.seconds;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(secs));
            let _ = guard.shutdown(std::net::Shutdown::Both);
        });
    }

    let mut r = FrameReader::new(BufReader::new(sock));
    let deadline =
        (args.seconds > 0).then(|| Instant::now() + Duration::from_secs(args.seconds));

    let mut lat_event: Vec<u64> = Vec::new();
    let mut lat_send: Vec<u64> = Vec::new();
    let mut frames = 0u64;
    let mut rects = 0u64;
    let mut wire_total = 0u64;
    let mut raw_total = 0u64;
    let mut decode_ns_total = 0u64;
    let mut decode_buf = Vec::new();
    let start = Instant::now();
    let mut interval = Instant::now();

    loop {
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break;
            }
        }
        let (ty, payload) = match r.read() {
            Ok(Some(frame)) => frame,
            Ok(None) => break, // EOF
            Err(e) => {
                eprintln!("[sink] read ended: {e}");
                break;
            }
        };
        match ty {
            MSG_HELLO if payload.len() >= 6 => {
                let ver = u16::from_le_bytes(payload[..2].try_into().unwrap());
                eprintln!("[sink] hello: proto v{ver}");
            }
            MSG_HELLO => {}
            MSG_DAMAGE => {
                let t_arrival = now_ns();
                let frame = DamageFrame::decode(&payload)?;
                lat_event.push(t_arrival.saturating_sub(frame.t_event_ns));
                lat_send.push(t_arrival.saturating_sub(frame.t_capture_ns));
                frames += 1;
                for r in &frame.rects {
                    rects += 1;
                    wire_total += r.data.len() as u64;
                    raw_total += r.raw_len as u64;
                    if args.decode {
                        let t0 = now_ns();
                        decompress(&mut decode_buf, r.codec, &r.data, r.raw_len as usize)?;
                        decode_ns_total += now_ns() - t0;
                    }
                }
            }
            _ => {}
        }

        if interval.elapsed() >= Duration::from_secs(1) {
            print_interval(
                frames,
                rects,
                wire_total,
                raw_total,
                decode_ns_total,
                &mut lat_event,
                &mut lat_send,
                start,
            );
            interval = Instant::now();
        }
    }

    print_interval(
        frames,
        rects,
        wire_total,
        raw_total,
        decode_ns_total,
        &mut lat_event,
        &mut lat_send,
        start,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)] // flat report table, not a call site
fn print_interval(
    frames: u64,
    rects: u64,
    wire_total: u64,
    raw_total: u64,
    decode_ns_total: u64,
    lat_event: &mut Vec<u64>,
    lat_send: &mut Vec<u64>,
    start: Instant,
) {
    let secs = start.elapsed().as_secs_f64().max(1e-6);
    let mut ev = std::mem::take(lat_event).into_boxed_slice();
    let mut sd = std::mem::take(lat_send).into_boxed_slice();
    ev.sort_unstable();
    sd.sort_unstable();
    eprintln!(
        "[sink] frames={frames} rects={rects} fps={:.1} in={:.2}MB/s ratio={:.1}x decode_avg={:.2}ms | lat(ev->arr) p50={}µs p90={}µs p99={}µs max={}µs | lat(enc->arr) p50={}µs p99={}µs",
        frames as f64 / secs,
        wire_total as f64 / 1e6 / secs,
        if wire_total > 0 { raw_total as f64 / wire_total as f64 } else { 0.0 },
        decode_ns_total as f64 / frames.max(1) as f64 / 1e6,
        pct(&ev, 0.50) / 1000,
        pct(&ev, 0.90) / 1000,
        pct(&ev, 0.99) / 1000,
        pct(&ev, 1.0) / 1000,
        pct(&sd, 0.50) / 1000,
        pct(&sd, 0.99) / 1000,
    );
    *lat_event = ev.into_vec();
    *lat_send = sd.into_vec();
}

// ---------------------------------------------------------------------------
// selftest: small-rect capture correctness (the backspace-artifact class)
// ---------------------------------------------------------------------------

fn selftest(args: SelftestArgs) -> Result<()> {
    use xcom_codec::decompress;
    use xcom_x11::capture::{CaptureConfig, CaptureSession};

    let spawned;
    let display = if args.display.is_empty() {
        spawned = Some(xcom_x11::xvfb::Xvfb::spawn(320, 240)?);
        spawned.as_ref().unwrap().display.clone()
    } else {
        args.display.clone()
    };
    let (conn, screen_num) = {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match x11rb::connect(Some(display.as_str())) {
                Ok(c) => break c,
                Err(e) if Instant::now() < deadline => {
                    if xcom_proto::debug() {
                        eprintln!("[selftest] retrying connect to {display}: {e}");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(e).with_context(|| format!("connecting to {display}")),
            }
        }
    };
    let root = conn.setup().roots[screen_num].root;

    let win: Window = conn.generate_id()?;
    conn.create_window(
        0,
        win,
        root,
        0,
        0,
        200,
        100,
        0,
        WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new().background_pixel(0),
    )?;
    conn.map_window(win)?;
    conn.flush()?;
    std::thread::sleep(Duration::from_millis(100));

    let mut session = CaptureSession::from_connection(conn, screen_num, CaptureConfig::default())?;
    let draw_conn = session.connection();
    session.add_window(win)?;

    let (w, h) = session
        .window_geometry(win)
        .map(|(_, _, w, h)| (w, h))
        .context("geometry")?;
    let mut store = vec![0u32; w as usize * h as usize];

    let apply = |store: &mut Vec<u32>,
                 w: u16,
                 frame: &xcom_proto::DamageFrame,
                 buf: &mut Vec<u8>| {
        for r in &frame.rects {
            let _ = decompress(buf, r.codec, &r.data, r.raw_len as usize);
            for row in 0..r.h as usize {
                let dy = r.y as usize + row;
                if dy >= store.len() / w as usize {
                    break;
                }
                for col in 0..r.w as usize {
                    let off = (row * r.w as usize + col) * 4;
                    if off + 4 > buf.len() {
                        break;
                    }
                    let px = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                    store[dy * w as usize + r.x as usize + col] = px;
                }
            }
        }
    };
    let pixel = |store: &[u32], x: u16, y: u16, w: u16| store[y as usize * w as usize + x as usize] & 0x00ff_ffff;
    let mut buf = Vec::new();

    // Baseline: the whole window is black.
    let (frame, _) = session.capture_full(win)?.context("initial full capture")?;
    apply(&mut store, w, &frame, &mut buf);
    if pixel(&store, 10, 10, w) != 0 {
        bail!("selftest: baseline is not black");
    }

    // Draw one small white cell (like a character) and wait for damage.
    draw_conn.poly_fill_rectangle(win, gc_of(&draw_conn, win, 0x00ff_ffff)?, &[Rectangle { x: 50, y: 20, width: 8, height: 16 }])?;
    draw_conn.flush()?;
    let (frame, _) = wait_frame(&mut session, Duration::from_secs(2))?
        .context("no damage frame after drawing the cell")?;
    let covered = frame
        .rects
        .iter()
        .any(|r| r.x <= 50 && r.y <= 20 && r.x as u32 + r.w as u32 >= 58 && r.y as u32 + r.h as u32 >= 36);
    apply(&mut store, w, &frame, &mut buf);
    if !covered {
        bail!("selftest: damage rect does not cover the drawn cell: {:?}", frame.rects.iter().map(|r| (r.x, r.y, r.w, r.h)).collect::<Vec<_>>());
    }
    if pixel(&store, 52, 25, w) != 0xffffff {
        bail!("selftest: drawn cell is not white after incremental capture");
    }

    // Erase it again (the backspace case): the cell must become black.
    draw_conn.poly_fill_rectangle(win, gc_of(&draw_conn, win, 0)?, &[Rectangle { x: 50, y: 20, width: 8, height: 16 }])?;
    draw_conn.flush()?;
    let (frame, _) = wait_frame(&mut session, Duration::from_secs(2))?
        .context("no damage frame after erasing the cell")?;
    apply(&mut store, w, &frame, &mut buf);
    if pixel(&store, 52, 25, w) != 0 {
        bail!(
            "selftest: erased cell still shows content ({:#08x}); damage rects: {:?}",
            pixel(&store, 52, 25, w),
            frame.rects.iter().map(|r| (r.x, r.y, r.w, r.h)).collect::<Vec<_>>()
        );
    }

    // The automated version of the F12 test: what the incremental damage path
    // produced must equal a full capture of the same window. Any difference is
    // a lost or undersized damage rect (thin slivers of erased text).
    let mut full_store = vec![0u32; w as usize * h as usize];
    let (full, _) = session.capture_full(win)?.context("final full capture")?;
    apply(&mut full_store, w, &full, &mut buf);
    let mut diff = 0usize;
    let mut bbox = (w, h, 0u16, 0u16);
    for y in 0..h {
        for x in 0..w {
            let i = y as usize * w as usize + x as usize;
            if store[i] & 0x00ff_ffff != full_store[i] & 0x00ff_ffff {
                diff += 1;
                bbox = (bbox.0.min(x), bbox.1.min(y), bbox.2.max(x), bbox.3.max(y));
            }
        }
    }
    if diff > 0 {
        bail!(
            "selftest: incremental store differs from full capture in {diff} pixels, bbox {:?} (lost damage)",
            bbox
        );
    }

    // H2: unmap is not destroy. The window must be reported hidden, produce no
    // frames while hidden, and be re-announced (and capturable) after remap.
    draw_conn.unmap_window(win)?;
    draw_conn.flush()?;
    if !wait_pump(&mut session, Duration::from_secs(2), |e| {
        matches!(e, PumpEvent::Unmapped(w) if *w == win)
    })? {
        bail!("selftest: unmap was not reported");
    }
    if session.capture_full(win)?.is_some() {
        bail!("selftest: an unmapped window produced a full frame");
    }
    draw_conn.map_window(win)?;
    draw_conn.flush()?;
    if !wait_pump(&mut session, Duration::from_secs(2), |e| {
        matches!(e, PumpEvent::Mapped(w) if *w == win)
    })? {
        bail!("selftest: remap was not re-announced");
    }
    session
        .capture_full(win)?
        .context("capture_full after remap")?;

    // Modifier ownership: with Shift held by the user, typing "A" must not
    // release Shift (the old injector did, so every capital letter flipped the
    // remote modifier state).
    let keymap = Keymap::query(&*draw_conn)?;
    let shift = keymap
        .resolve(xcom_x11::keymap::XK_SHIFT_L)
        .context("remote keymap has no Shift_L")?;
    let injector = Injector::new(&*draw_conn, win, keymap);
    let shift_down = |conn: &x11rb::rust_connection::RustConnection| -> Result<bool> {
        let reply = conn.query_keymap()?.reply()?;
        Ok(reply.keys[shift.keycode as usize / 8] & (1 << (shift.keycode % 8)) != 0)
    };
    injector.key(xcom_x11::keymap::XK_SHIFT_L, true)?;
    injector.key(0x41, true)?; // 'A' needs Shift
    injector.key(0x41, false)?;
    if !shift_down(&draw_conn)? {
        bail!("selftest: typing 'A' released the user's held Shift");
    }
    injector.key(xcom_x11::keymap::XK_SHIFT_L, false)?;
    if shift_down(&draw_conn)? {
        bail!("selftest: Shift stayed down after its own release");
    }

    // Unicode keysyms the layout lacks get stable scratch keycodes. A rapid
    // é/à sequence must be read back with those keysyms: remapping one
    // keycode between presses let xterm translate é with à's mapping.
    draw_conn.change_window_attributes(
        win,
        &x11rb::protocol::xproto::ChangeWindowAttributesAux::new()
            .event_mask(EventMask::KEY_PRESS),
    )?;
    draw_conn.flush()?;
    injector.focus(win)?;
    for sym in [0xE9u32, 0xE0, 0xE9, 0xE0] {
        injector.key(sym, true)?;
        injector.key(sym, false)?;
    }
    let expected = vec![0xE9u32, 0xE0, 0xE9, 0xE0];
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(2);
    while seen.len() < expected.len() && Instant::now() < deadline {
        while let Some(ev) = draw_conn.poll_for_event()? {
            if let Event::KeyPress(e) = ev {
                let map = draw_conn.get_keyboard_mapping(e.detail, 1)?.reply()?;
                seen.push(map.keysyms.first().copied().unwrap_or(0));
            }
        }
        if seen.len() < expected.len() {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    if seen != expected {
        bail!("selftest: unicode keysyms came back as {seen:x?}, expected {expected:x?}");
    }

    println!(
        "SELFTEST PASS: draw+erase correct ({}x{}), incremental == full, unmap/remap kept the window, held modifiers preserved, unicode keycodes stable",
        w, h
    );
    Ok(())
}

/// Pump until an event matches or the timeout expires.
fn wait_pump<F>(session: &mut CaptureSession, timeout: Duration, pred: F) -> Result<bool>
where
    F: Fn(&PumpEvent) -> bool,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(ev) = session.pump(Some(deadline))? {
            if pred(&ev) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn gc_of(conn: &x11rb::rust_connection::RustConnection, win: Window, fg: u32) -> Result<Gcontext> {
    let gc: Gcontext = conn.generate_id()?;
    conn.create_gc(gc, win, &CreateGCAux::new().foreground(fg).graphics_exposures(0u32))?;
    conn.flush()?;
    Ok(gc)
}

fn wait_frame(
    session: &mut xcom_x11::capture::CaptureSession,
    timeout: Duration,
) -> Result<Option<(xcom_proto::DamageFrame, xcom_x11::capture::FrameStats)>> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(PumpEvent::Frame { frame, stats, .. }) =
            session.pump(Some(deadline))?
        {
            return Ok(Some((frame, stats)));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// input-test
// ---------------------------------------------------------------------------

#[derive(clap::Args)]
pub struct SelftestArgs {
    /// Existing display; default: spawn a private 320x240 one.
    #[arg(long, default_value = "")]
    pub display: String,
}

#[derive(clap::Args)]
pub struct InputTestArgs {
    #[arg(long, default_value = ":9")]
    pub display: String,
    /// Window to focus and type into (hex id or auto:first / auto:<name>).
    #[arg(long)]
    pub window: String,
    /// Log file written by `m0 anim --input-log`.
    #[arg(long)]
    pub check_log: String,
}

fn input_test(args: InputTestArgs) -> Result<()> {
    let (conn, screen_num) = x11rb::connect(Some(&args.display))?;
    let root = conn.setup().roots[screen_num].root;
    let keymap = Keymap::query(&conn)?;
    let inj = Injector::new(&conn, root, keymap);

    let win = resolve_window(&conn, root, &args.window)
        .with_context(|| format!("cannot resolve window {:?}", args.window))?;
    inj.focus(win)?;
    std::thread::sleep(Duration::from_millis(100));

    // Latin-1 chars plus arrows: covers plain, shifted and modifier keys.
    let mut sequence: Vec<u32> = "abcAZ1!@ ".chars().map(|c| c as u32).collect();
    sequence.extend_from_slice(&[0xff51, 0xff52, 0xff53, 0xff54, XK_RETURN]);

    let mut sent: Vec<(u32, u64)> = Vec::new();
    for &sym in &sequence {
        let t = now_ns();
        inj.tap(sym)?;
        conn.flush()?;
        sent.push((sym, t));
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(300));

    let log = std::fs::read_to_string(&args.check_log)
        .with_context(|| format!("reading {}", args.check_log))?;
    let mut recvs: Vec<(u32, u64)> = Vec::new();
    for line in log.lines() {
        // recv sym=0x61 code=38 state=0x0 t_ns=123
        let mut sym = 0u32;
        let mut t = 0u64;
        for field in line.split_whitespace() {
            if let Some(v) = field.strip_prefix("sym=") {
                sym = u32::from_str_radix(v.trim_start_matches("0x"), 16).unwrap_or(0);
            } else if let Some(v) = field.strip_prefix("t_ns=") {
                t = v.parse().unwrap_or(0);
            }
        }
        // Injected modifier keycodes (Shift for 'A', ...) show up in the app's
        // log as key presses of their own; they are not part of `sent`.
        if is_modifier_sym(sym) {
            continue;
        }
        recvs.push((sym, t));
    }

    println!("sent {} keys, app reported {}", sent.len(), recvs.len());
    let mut lats: Vec<u64> = Vec::new();
    let mut missing = 0;
    for (i, &(sym, t_sent)) in sent.iter().enumerate() {
        match recvs.get(i) {
            Some(&(got, t_recv)) if got == sym => {
                let lat = t_recv.saturating_sub(t_sent);
                lats.push(lat);
                println!(
                    "  ok  sym={sym:#07x} latency={:.1}µs",
                    lat as f64 / 1000.0
                );
            }
            Some(&(got, _)) => {
                missing += 1;
                println!("  MISMATCH sym={sym:#07x} but app saw {got:#07x}");
            }
            None => {
                missing += 1;
                println!("  MISSING sym={sym:#07x}");
            }
        }
    }
    lats.sort_unstable();
    println!(
        "RESULT ok={} bad={} latency p50={:.1}µs p90={:.1}µs max={:.1}µs",
        lats.len(),
        missing,
        pct(&lats, 0.5) as f64 / 1000.0,
        pct(&lats, 0.9) as f64 / 1000.0,
        pct(&lats, 1.0) as f64 / 1000.0,
    );
    if missing > 0 {
        bail!("{missing} injected keys did not arrive as typed");
    }
    Ok(())
}
