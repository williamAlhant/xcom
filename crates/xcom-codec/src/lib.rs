//! Damage-rect coalescing and pixel compression.
//!
//! Wire cost is driven by two things: how many rectangles a frame describes and
//! how well their pixels compress. `coalesce` turns the raw damage stream from
//! the X server into a small set of disjoint rects; [`Compressor`] compresses
//! each rect's pixels with LZ4 and falls back to raw bytes when compression
//! does not pay for itself (video, noise, already-compressed content).

/// A rectangle in window coordinates. Zero width or height means empty.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub const fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }

    pub const fn area(self) -> usize {
        self.w as usize * self.h as usize
    }

    pub const fn is_empty(self) -> bool {
        self.w == 0 || self.h == 0
    }

    const fn right(self) -> u32 {
        self.x as u32 + self.w as u32
    }

    const fn bottom(self) -> u32 {
        self.y as u32 + self.h as u32
    }

    /// True when the rects overlap or touch along an edge or corner. Such rects
    /// can be unioned without enclosing extra undamaged pixels, so merging them
    /// always reduces the number of rects we have to describe on the wire.
    pub const fn touches(self, o: Rect) -> bool {
        self.right() >= o.x as u32
            && o.right() >= self.x as u32
            && self.bottom() >= o.y as u32
            && o.bottom() >= self.y as u32
    }

    pub const fn union(self, o: Rect) -> Rect {
        if self.is_empty() {
            return o;
        }
        if o.is_empty() {
            return self;
        }
        let x = if self.x < o.x { self.x } else { o.x };
        let y = if self.y < o.y { self.y } else { o.y };
        let r = if self.right() > o.right() {
            self.right()
        } else {
            o.right()
        };
        let b = if self.bottom() > o.bottom() {
            self.bottom()
        } else {
            o.bottom()
        };
        Rect::new(x, y, (r - x as u32) as u16, (b - y as u32) as u16)
    }

    /// Grow by `n` pixels on every side (saturating; clamp afterwards).
    pub const fn padded(self, n: u16) -> Rect {
        Rect::new(
            self.x.saturating_sub(n),
            self.y.saturating_sub(n),
            self.w.saturating_add(n.saturating_mul(2)),
            self.h.saturating_add(n.saturating_mul(2)),
        )
    }

    /// Clamp to a `w`x`h` drawable; returns an empty rect when fully outside.
    pub const fn clamped(self, w: u16, h: u16) -> Rect {
        let x = if self.x > w { w } else { self.x };
        let y = if self.y > h { h } else { self.y };
        let r = if self.right() > w as u32 {
            w as u32
        } else {
            self.right()
        };
        let b = if self.bottom() > h as u32 {
            h as u32
        } else {
            self.bottom()
        };
        if r <= x as u32 || b <= y as u32 {
            Rect::new(x, y, 0, 0)
        } else {
            Rect::new(x, y, (r - x as u32) as u16, (b - y as u32) as u16)
        }
    }
}

/// Merge damage rects so a frame carries at most `max_rects` of them.
///
/// Pass 1 unions everything that overlaps or touches. Pass 2, only if the rect
/// count still exceeds `max_rects`, repeatedly merges the pair whose union adds
/// the least undamaged area, so the wasted bandwidth stays minimal.
pub fn coalesce(rects: &mut Vec<Rect>, max_rects: usize) {
    rects.retain(|r| !r.is_empty());

    loop {
        let mut merged = false;
        'pass: for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                if rects[i].touches(rects[j]) {
                    rects[i] = rects[i].union(rects[j]);
                    rects.swap_remove(j);
                    merged = true;
                    break 'pass;
                }
            }
        }
        if !merged {
            break;
        }
    }

    while rects.len() > max_rects {
        let mut best = (0usize, 1usize);
        let mut best_cost = usize::MAX;
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                let cost = rects[i].union(rects[j]).area() - rects[i].area() - rects[j].area();
                if cost < best_cost {
                    best_cost = cost;
                    best = (i, j);
                }
            }
        }
        let (i, j) = best;
        rects[i] = rects[i].union(rects[j]);
        rects.swap_remove(j);
    }
}

pub const CODEC_RAW: u8 = 0;
pub const CODEC_LZ4: u8 = 1;

/// LZ4 block compressor over a reusable buffer.
pub struct Compressor {
    buf: Vec<u8>,
}

impl Compressor {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Compress `src` (a BGRA8 rect) and return `(codec, bytes)`.
    ///
    /// Chooses raw whenever LZ4 fails to save at least ~3% plus a small
    /// constant: encoding incompressible pixels wastes CPU on both ends and
    /// grows the payload slightly.
    pub fn encode<'a>(&'a mut self, src: &'a [u8]) -> (u8, &'a [u8]) {
        // lz4_flex refuses to compress unless the output buffer is at least
        // get_maximum_output_size(), regardless of how well the data compresses.
        let cap = lz4_flex::block::get_maximum_output_size(src.len());
        self.buf.clear();
        self.buf.resize(cap, 0);
        match lz4_flex::block::compress_into(src, &mut self.buf) {
            Ok(n) if n + src.len() / 32 + 32 < src.len() => {
                self.buf.truncate(n);
                (CODEC_LZ4, &self.buf)
            }
            _ => {
                self.buf.clear();
                (CODEC_RAW, src)
            }
        }
    }

    /// Compressed bytes produced by the last [`encode`] call (or raw pass-through).
    pub fn buffer(&self) -> &[u8] {
        &self.buf
    }
}

impl Default for Compressor {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum CodecError {
    UnknownCodec(u8),
    Decompress(lz4_flex::block::DecompressError),
    SizeMismatch { expected: usize, got: usize },
}

impl std::fmt::Display for CodecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodecError::UnknownCodec(c) => write!(f, "unknown codec {c}"),
            CodecError::Decompress(e) => write!(f, "lz4 decompress: {e}"),
            CodecError::SizeMismatch { expected, got } => {
                write!(f, "decompressed {got} bytes, expected {expected}")
            }
        }
    }
}

impl std::error::Error for CodecError {}

/// Decompress one rect into `dst` (cleared first). `raw_len` is the pixel byte
/// count of the rect, used as the LZ4 output bound and for validation.
pub fn decompress(dst: &mut Vec<u8>, codec: u8, src: &[u8], raw_len: usize) -> Result<(), CodecError> {
    dst.clear();
    match codec {
        CODEC_RAW => {
            if src.len() != raw_len {
                return Err(CodecError::SizeMismatch {
                    expected: raw_len,
                    got: src.len(),
                });
            }
            dst.extend_from_slice(src);
        }
        CODEC_LZ4 => {
            let out = lz4_flex::block::decompress(src, raw_len).map_err(CodecError::Decompress)?;
            if out.len() != raw_len {
                return Err(CodecError::SizeMismatch {
                    expected: raw_len,
                    got: out.len(),
                });
            }
            dst.extend_from_slice(&out);
        }
        other => return Err(CodecError::UnknownCodec(other)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rect_union_and_touch() {
        let a = Rect::new(0, 0, 10, 10);
        let b = Rect::new(10, 5, 4, 4);
        assert!(a.touches(b));
        assert_eq!(a.union(b), Rect::new(0, 0, 14, 10));
        let c = Rect::new(20, 20, 2, 2);
        assert!(!a.touches(c));
    }

    #[test]
    fn coalesce_merges_touching() {
        let mut v = vec![
            Rect::new(0, 0, 8, 8),
            Rect::new(8, 0, 8, 8),
            Rect::new(100, 100, 4, 4),
        ];
        coalesce(&mut v, 16);
        assert_eq!(v.len(), 2);
    }

    #[test]
    fn coalesce_caps_count() {
        let mut v: Vec<Rect> = (0..32u16).map(|i| Rect::new(i * 20, 0, 4, 4)).collect();
        coalesce(&mut v, 4);
        assert!(v.len() <= 4);
    }

    #[test]
    fn lz4_roundtrip() {
        let mut c = Compressor::new();
        let src = vec![7u8; 4096];
        let (codec, bytes) = c.encode(&src);
        assert_eq!(codec, CODEC_LZ4);
        let mut out = Vec::new();
        decompress(&mut out, codec, bytes, src.len()).unwrap();
        assert_eq!(out, src);
    }

    #[test]
    fn raw_fallback_for_noise() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut c = Compressor::new();
        let src: Vec<u8> = (0..8192u64)
            .map(|i| {
                let mut h = DefaultHasher::new();
                i.hash(&mut h);
                h.finish() as u8
            })
            .collect();
        let (codec, bytes) = c.encode(&src);
        assert_eq!(codec, CODEC_RAW);
        assert_eq!(bytes, &src[..]);
    }
}
