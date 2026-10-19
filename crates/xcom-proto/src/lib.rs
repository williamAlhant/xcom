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
pub struct DamageFrame {
    pub window: u32,
    /// Monotonic ns when the first damage event of this batch was seen.
    pub t_event_ns: u64,
    /// Monotonic ns after capture+encode of the whole frame.
    pub t_capture_ns: u64,
    pub rects: Vec<WireRect>,
}

impl DamageFrame {
    pub fn encoded_len(&self) -> usize {
        4 + 8 + 8 + 2 + self.rects.iter().map(|r| 17 + r.data.len()).sum::<usize>()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.encoded_len());
        out.extend_from_slice(&self.window.to_le_bytes());
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
}

impl std::fmt::Display for ProtoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProtoError::Truncated => write!(f, "truncated message"),
            ProtoError::Io(e) => write!(f, "io: {e}"),
            ProtoError::EmptyFrame => write!(f, "empty frame"),
        }
    }
}

impl std::error::Error for ProtoError {}

impl From<std::io::Error> for ProtoError {
    fn from(e: std::io::Error) -> Self {
        ProtoError::Io(e)
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
        let mut hdr = [0u8; 5];
        hdr[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        hdr[4] = msg_type;
        self.inner.write_all(&hdr)?;
        self.inner.write_all(payload)?;
        Ok(())
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
}

impl<R: std::io::Read> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// Returns `None` on clean EOF between frames.
    pub fn read(&mut self) -> Result<Option<(u8, Vec<u8>)>, ProtoError> {
        let mut hdr = [0u8; 5];
        match self.inner.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(e.into()),
        }
        let len = u32::from_le_bytes(hdr[..4].try_into().unwrap()) as usize;
        if len == 0 {
            return Err(ProtoError::EmptyFrame);
        }
        if len > 512 << 20 {
            return Err(ProtoError::Truncated);
        }
        let mut payload = vec![0u8; len];
        self.inner.read_exact(&mut payload)?;
        Ok(Some((hdr[4], payload)))
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
        assert_eq!(g.t_event_ns, 111);
        assert_eq!(g.rects.len(), 1);
        assert_eq!(g.rects[0].data, vec![7, 8, 9]);
    }
}
