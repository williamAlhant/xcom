//! Wire protocol for the xcom channel (version 1).
//!
//! Framing is hand-rolled and versioned: `u32le payload_len | u8 msg_type |
//! payload`. Pixel payloads stay byte-oriented (no serialization framework in
//! the hot path) so a damage frame can carry compressed rect data without an
//! extra copy. Control messages will move to `postcard` when they grow past
//! hello/ping.
//!
//! Timestamps use `CLOCK_MONOTONIC` nanoseconds, which on Linux is comparable
//! across processes on the same host — that lets the M0 spike measure true
//! one-way latency between the capture process and the sink.

pub const PROTO_VERSION: u16 = 1;

pub const MSG_HELLO: u8 = 1;
pub const MSG_DAMAGE: u8 = 2;
pub const MSG_PING: u8 = 3;
pub const MSG_PONG: u8 = 4;
pub const MSG_AUTH_OK: u8 = 5;
pub const MSG_AUTH_REJECTED: u8 = 6;

// Window lifecycle (server -> client).
pub const MSG_WINDOW_CREATE: u8 = 10;
pub const MSG_WINDOW_DESTROY: u8 = 11;
pub const MSG_WINDOW_GEOMETRY: u8 = 12;

// Input and control (client -> server).
pub const MSG_FOCUS: u8 = 19;
pub const MSG_INPUT_KEY: u8 = 20;
pub const MSG_INPUT_BUTTON: u8 = 21;
pub const MSG_INPUT_MOTION: u8 = 22;
pub const MSG_RESIZE: u8 = 23;
pub const MSG_FULL_REQUEST: u8 = 24;
pub const MSG_BYE: u8 = 25;

/// `XCOM_DEBUG=1` enables protocol/input tracing (test and diagnosis aid).
pub fn debug() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("XCOM_DEBUG").is_some_and(|v| v != "0" && !v.is_empty())
    })
}

/// Monotonic nanoseconds, comparable between processes on the same host.
pub fn now_ns() -> u64 {
    unsafe {
        let mut ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
        ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
    }
}

/// One damaged rectangle's pixels on the wire.
#[derive(Debug)]
pub struct WireRect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub codec: u8,
    /// Uncompressed pixel byte count (`w * h * 4` for BGRA8).
    pub raw_len: u32,
    pub data: Vec<u8>,
}

/// One damage batch: everything that was dirty in a window when we sampled it.
///
/// Frames are self-describing: `win_w`/`win_h` are the window's size at capture
/// time and the blit stride, so a frame is correct even when it arrives before
/// its `WindowCreate` (the two channels are not ordered against each other) or
/// while a resize is in flight.
#[derive(Debug)]
pub struct DamageFrame {
    pub window: u32,
    pub win_w: u16,
    pub win_h: u16,
    /// Monotonic ns when the first damage event of this batch was seen.
    pub t_event_ns: u64,
    /// Monotonic ns after capture+encode of the whole frame.
    pub t_capture_ns: u64,
    pub rects: Vec<WireRect>,
}

impl DamageFrame {
    pub fn encoded_len(&self) -> usize {
        4 + 2 + 2 + 8 + 8 + 2 + self.rects.iter().map(|r| 17 + r.data.len()).sum::<usize>()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(&self.window.to_le_bytes());
        out.extend_from_slice(&self.win_w.to_le_bytes());
        out.extend_from_slice(&self.win_h.to_le_bytes());
        out.extend_from_slice(&self.t_event_ns.to_le_bytes());
        out.extend_from_slice(&self.t_capture_ns.to_le_bytes());
        out.extend_from_slice(&(self.rects.len() as u16).to_le_bytes());
        for r in &self.rects {
            out.extend_from_slice(&r.x.to_le_bytes());
            out.extend_from_slice(&r.y.to_le_bytes());
            out.extend_from_slice(&r.w.to_le_bytes());
            out.extend_from_slice(&r.h.to_le_bytes());
            out.push(r.codec);
            out.extend_from_slice(&r.raw_len.to_le_bytes());
            out.extend_from_slice(&(r.data.len() as u32).to_le_bytes());
            out.extend_from_slice(&r.data);
        }
        out
    }

    pub fn decode(payload: &[u8]) -> Result<DamageFrame, ProtoError> {
        let mut p = payload;
        let mut take = |n: usize| -> Result<&[u8], ProtoError> {
            if p.len() < n {
                return Err(ProtoError::Truncated);
            }
            let (a, b) = p.split_at(n);
            p = b;
            Ok(a)
        };
        let window = u32::from_le_bytes(take(4)?.try_into().unwrap());
        let win_w = u16::from_le_bytes(take(2)?.try_into().unwrap());
        let win_h = u16::from_le_bytes(take(2)?.try_into().unwrap());
        let t_event_ns = u64::from_le_bytes(take(8)?.try_into().unwrap());
        let t_capture_ns = u64::from_le_bytes(take(8)?.try_into().unwrap());
        let nrects = u16::from_le_bytes(take(2)?.try_into().unwrap());
        let mut rects = Vec::with_capacity(nrects as usize);
        for _ in 0..nrects {
            let x = u16::from_le_bytes(take(2)?.try_into().unwrap());
            let y = u16::from_le_bytes(take(2)?.try_into().unwrap());
            let w = u16::from_le_bytes(take(2)?.try_into().unwrap());
            let h = u16::from_le_bytes(take(2)?.try_into().unwrap());
            let codec = take(1)?[0];
            let raw_len = u32::from_le_bytes(take(4)?.try_into().unwrap());
            let len = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
            let data = take(len)?.to_vec();
            rects.push(WireRect {
                x,
                y,
                w,
                h,
                codec,
                raw_len,
                data,
            });
        }
        Ok(DamageFrame {
            window,
            win_w,
            win_h,
            t_event_ns,
            t_capture_ns,
            rects,
        })
    }
}

#[derive(Debug)]
pub enum ProtoError {
    Truncated,
    Io(std::io::Error),
    EmptyFrame,
    BadMessage(&'static str),
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoError::Truncated => write!(f, "truncated message"),
            ProtoError::Io(e) => write!(f, "io: {e}"),
            ProtoError::EmptyFrame => write!(f, "empty frame"),
            ProtoError::BadMessage(m) => write!(f, "bad message: {m}"),
        }
    }
}

impl std::error::Error for ProtoError {}

/// Everything except damage frames (which carry raw pixel payloads and stay
/// on the fast [`DamageFrame`] path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    WindowCreate {
        id: u32,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        title: String,
    },
    WindowDestroy {
        id: u32,
    },
    WindowGeometry {
        id: u32,
        w: u16,
        h: u16,
    },
    /// Keyboard focus moved to this remote window (client -> server).
    Focus {
        window: u32,
    },
    InputKey {
        sym: u32,
        pressed: bool,
    },
    InputButton {
        window: u32,
        button: u8,
        pressed: bool,
        x: i16,
        y: i16,
    },
    InputMotion {
        window: u32,
        x: i16,
        y: i16,
    },
    Resize {
        id: u32,
        w: u16,
        h: u16,
    },
    FullRequest {
        id: u32,
    },
    Ping {
        t: u64,
    },
    Pong {
        t: u64,
    },
    Bye,
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_i16(out: &mut Vec<u8>, v: i16) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn take<'a>(p: &mut &'a [u8], n: usize) -> Result<&'a [u8], ProtoError> {
    if p.len() < n {
        return Err(ProtoError::Truncated);
    }
    let (a, b) = p.split_at(n);
    *p = b;
    Ok(a)
}

fn take_u16(p: &mut &[u8]) -> Result<u16, ProtoError> {
    Ok(u16::from_le_bytes(take(p, 2)?.try_into().unwrap()))
}

fn take_i16(p: &mut &[u8]) -> Result<i16, ProtoError> {
    Ok(i16::from_le_bytes(take(p, 2)?.try_into().unwrap()))
}

fn take_u32(p: &mut &[u8]) -> Result<u32, ProtoError> {
    Ok(u32::from_le_bytes(take(p, 4)?.try_into().unwrap()))
}

fn take_u64(p: &mut &[u8]) -> Result<u64, ProtoError> {
    Ok(u64::from_le_bytes(take(p, 8)?.try_into().unwrap()))
}

fn take_str(p: &mut &[u8]) -> Result<String, ProtoError> {
    let len = take_u16(p)? as usize;
    let bytes = take(p, len)?;
    String::from_utf8(bytes.to_vec()).map_err(|_| ProtoError::BadMessage("title not utf8"))
}

impl Message {
    pub fn msg_type(&self) -> u8 {
        match self {
            Message::WindowCreate { .. } => MSG_WINDOW_CREATE,
            Message::WindowDestroy { .. } => MSG_WINDOW_DESTROY,
            Message::WindowGeometry { .. } => MSG_WINDOW_GEOMETRY,
            Message::Focus { .. } => MSG_FOCUS,
            Message::InputKey { .. } => MSG_INPUT_KEY,
            Message::InputButton { .. } => MSG_INPUT_BUTTON,
            Message::InputMotion { .. } => MSG_INPUT_MOTION,
            Message::Resize { .. } => MSG_RESIZE,
            Message::FullRequest { .. } => MSG_FULL_REQUEST,
            Message::Ping { .. } => MSG_PING,
            Message::Pong { .. } => MSG_PONG,
            Message::Bye => MSG_BYE,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32);
        match self {
            Message::WindowCreate {
                id,
                x,
                y,
                w,
                h,
                title,
            } => {
                put_u32(&mut out, *id);
                put_i16(&mut out, *x);
                put_i16(&mut out, *y);
                put_u16(&mut out, *w);
                put_u16(&mut out, *h);
                put_u16(&mut out, title.len() as u16);
                out.extend_from_slice(title.as_bytes());
            }
            Message::WindowDestroy { id } => put_u32(&mut out, *id),
            Message::WindowGeometry { id, w, h } => {
                put_u32(&mut out, *id);
                put_u16(&mut out, *w);
                put_u16(&mut out, *h);
            }
            Message::Focus { window } => put_u32(&mut out, *window),
            Message::InputKey { sym, pressed } => {
                put_u32(&mut out, *sym);
                out.push(*pressed as u8);
            }
            Message::InputButton {
                window,
                button,
                pressed,
                x,
                y,
            } => {
                put_u32(&mut out, *window);
                out.push(*button);
                out.push(*pressed as u8);
                put_i16(&mut out, *x);
                put_i16(&mut out, *y);
            }
            Message::InputMotion { window, x, y } => {
                put_u32(&mut out, *window);
                put_i16(&mut out, *x);
                put_i16(&mut out, *y);
            }
            Message::Resize { id, w, h } => {
                put_u32(&mut out, *id);
                put_u16(&mut out, *w);
                put_u16(&mut out, *h);
            }
            Message::FullRequest { id } => put_u32(&mut out, *id),
            Message::Ping { t } | Message::Pong { t } => put_u64(&mut out, *t),
            Message::Bye => {}
        }
        out
    }

    pub fn decode(msg_type: u8, payload: &[u8]) -> Result<Message, ProtoError> {
        let mut p = payload;
        Ok(match msg_type {
            MSG_WINDOW_CREATE => Message::WindowCreate {
                id: take_u32(&mut p)?,
                x: take_i16(&mut p)?,
                y: take_i16(&mut p)?,
                w: take_u16(&mut p)?,
                h: take_u16(&mut p)?,
                title: take_str(&mut p)?,
            },
            MSG_WINDOW_DESTROY => Message::WindowDestroy {
                id: take_u32(&mut p)?,
            },
            MSG_WINDOW_GEOMETRY => Message::WindowGeometry {
                id: take_u32(&mut p)?,
                w: take_u16(&mut p)?,
                h: take_u16(&mut p)?,
            },
            MSG_FOCUS => Message::Focus {
                window: take_u32(&mut p)?,
            },
            MSG_INPUT_KEY => Message::InputKey {
                sym: take_u32(&mut p)?,
                pressed: take(&mut p, 1)?[0] != 0,
            },
            MSG_INPUT_BUTTON => Message::InputButton {
                window: take_u32(&mut p)?,
                button: take(&mut p, 1)?[0],
                pressed: take(&mut p, 1)?[0] != 0,
                x: take_i16(&mut p)?,
                y: take_i16(&mut p)?,
            },
            MSG_INPUT_MOTION => Message::InputMotion {
                window: take_u32(&mut p)?,
                x: take_i16(&mut p)?,
                y: take_i16(&mut p)?,
            },
            MSG_RESIZE => Message::Resize {
                id: take_u32(&mut p)?,
                w: take_u16(&mut p)?,
                h: take_u16(&mut p)?,
            },
            MSG_FULL_REQUEST => Message::FullRequest {
                id: take_u32(&mut p)?,
            },
            MSG_PING => Message::Ping { t: take_u64(&mut p)? },
            MSG_PONG => Message::Pong { t: take_u64(&mut p)? },
            MSG_BYE => Message::Bye,
            _ => return Err(ProtoError::BadMessage("unknown message type")),
        })
    }

    /// Send on the channel and receive the decoded reply counterpart.
    pub fn send<W: std::io::Write>(&self, w: &mut W) -> Result<(), ProtoError> {
        write_frame(w, self.msg_type(), &self.encode())
    }
}

impl From<std::io::Error> for ProtoError {
    fn from(e: std::io::Error) -> Self {
        ProtoError::Io(e)
    }
}

/// Writes one framed message: `u32le payload_len | u8 msg_type | payload`.
pub fn write_frame<W: std::io::Write>(
    w: &mut W,
    msg_type: u8,
    payload: &[u8],
) -> Result<(), ProtoError> {
    let mut hdr = [0u8; 5];
    hdr[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    hdr[4] = msg_type;
    w.write_all(&hdr)?;
    w.write_all(payload)?;
    Ok(())
}

/// Resumable frame reader state (no I/O ownership).
///
/// Partial frames survive `WouldBlock`/`TimedOut`, which makes it safe to use
/// on sockets with read timeouts — the control channel polls with a short
/// timeout so it can interleave outbound writes.
#[derive(Default)]
pub struct FrameState {
    hdr: [u8; 5],
    hdr_have: usize,
    payload: Vec<u8>,
    payload_have: usize,
}

impl FrameState {
    /// Try to read one frame from `r`.
    ///
    /// `Ok(None)` = clean EOF between frames, `Err(Io(WouldBlock|TimedOut))` =
    /// nothing arrived yet (any partial frame is kept for the next call).
    pub fn read_from<R: std::io::Read>(
        &mut self,
        r: &mut R,
    ) -> Result<Option<(u8, Vec<u8>)>, ProtoError> {
        while self.hdr_have < self.hdr.len() {
            match r.read(&mut self.hdr[self.hdr_have..]) {
                Ok(0) if self.hdr_have == 0 => return Ok(None),
                Ok(0) => return Err(ProtoError::Truncated),
                Ok(n) => self.hdr_have += n,
                Err(e) => return Err(e.into()),
            }
        }
        if self.payload.is_empty() && self.payload_have == 0 {
            let len = u32::from_le_bytes(self.hdr[..4].try_into().unwrap()) as usize;
            if len > 512 << 20 {
                return Err(ProtoError::Truncated);
            }
            self.payload.resize(len, 0);
        }
        while self.payload_have < self.payload.len() {
            match r.read(&mut self.payload[self.payload_have..]) {
                Ok(0) => return Err(ProtoError::Truncated),
                Ok(n) => self.payload_have += n,
                Err(e) => return Err(e.into()),
            }
        }
        let msg_type = self.hdr[4];
        let payload = std::mem::take(&mut self.payload);
        self.hdr_have = 0;
        self.payload_have = 0;
        Ok(Some((msg_type, payload)))
    }
}

/// Writes length-prefixed frames to a stream.
pub struct FrameWriter<W> {
    inner: W,
}

impl<W: std::io::Write> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    pub fn write(&mut self, msg_type: u8, payload: &[u8]) -> Result<(), ProtoError> {
        write_frame(&mut self.inner, msg_type, payload)
    }

    pub fn flush(&mut self) -> Result<(), ProtoError> {
        self.inner.flush()?;
        Ok(())
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

/// Reads length-prefixed frames from a stream.
pub struct FrameReader<R> {
    inner: R,
    state: FrameState,
}

impl<R: std::io::Read> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            state: FrameState::default(),
        }
    }

    /// Blocking read of the next frame. Returns `None` on clean EOF.
    pub fn read(&mut self) -> Result<Option<(u8, Vec<u8>)>, ProtoError> {
        loop {
            match self.state.read_from(&mut self.inner) {
                Err(ProtoError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue;
                }
                other => return other,
            }
        }
    }

    /// One non-blocking attempt (state preserved on timeout).
    pub fn try_read(&mut self) -> Result<Option<(u8, Vec<u8>)>, ProtoError> {
        self.state.read_from(&mut self.inner)
    }

    pub fn into_inner(self) -> R {
        self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let buf: Vec<u8> = Vec::new();
        let mut w = FrameWriter::new(buf);
        w.write(MSG_PING, &[1, 2, 3]).unwrap();
        w.write(MSG_DAMAGE, &[9]).unwrap();
        let data = w.into_inner();
        let mut r = FrameReader::new(&data[..]);
        let (t, p) = r.read().unwrap().unwrap();
        assert_eq!((t, &p[..]), (MSG_PING, &[1, 2, 3][..]));
        let (t, p) = r.read().unwrap().unwrap();
        assert_eq!((t, &p[..]), (MSG_DAMAGE, &[9][..]));
        assert!(r.read().unwrap().is_none());
    }

    #[test]
    fn damage_roundtrip() {
        let f = DamageFrame {
            window: 42,
            win_w: 800,
            win_h: 600,
            t_event_ns: 111,
            t_capture_ns: 222,
            rects: vec![
                WireRect {
                    x: 1,
                    y: 2,
                    w: 3,
                    h: 4,
                    codec: 1,
                    raw_len: 48,
                    data: vec![7, 8, 9],
                },
            ],
        };
        let bytes = f.encode();
        let g = DamageFrame::decode(&bytes).unwrap();
        assert_eq!(g.window, 42);
        assert_eq!((g.win_w, g.win_h), (800, 600));
        assert_eq!(g.t_event_ns, 111);
        assert_eq!(g.rects.len(), 1);
        assert_eq!(g.rects[0].data, vec![7, 8, 9]);
    }

    #[test]
    fn message_roundtrip() {
        let msgs = vec![
            Message::WindowCreate {
                id: 7,
                x: -1,
                y: 2,
                w: 800,
                h: 600,
                title: "xterm — ü".to_string(),
            },
            Message::WindowDestroy { id: 7 },
            Message::WindowGeometry {
                id: 7,
                w: 1024,
                h: 768,
            },
            Message::Focus { window: 7 },
            Message::InputKey {
                sym: 0xff0d,
                pressed: true,
            },
            Message::InputButton {
                window: 7,
                button: 1,
                pressed: false,
                x: 10,
                y: 20,
            },
            Message::InputMotion {
                window: 7,
                x: -5,
                y: 5,
            },
            Message::Resize {
                id: 7,
                w: 640,
                h: 480,
            },
            Message::FullRequest { id: 7 },
            Message::Ping { t: 123456789 },
            Message::Pong { t: 123456789 },
            Message::Bye,
        ];
        for m in msgs {
            let bytes = m.encode();
            let back = Message::decode(m.msg_type(), &bytes).unwrap();
            assert_eq!(m, back);
        }
    }
}
