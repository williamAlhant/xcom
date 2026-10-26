//! Tile hashes for suppressing redundant pixels in large damage frames.
//!
//! Some toolkits and GPU drivers report a whole window as damaged on every
//! swap even when one cursor cell changed. The capture loop reads the full
//! window for those frames, then uses this grid to keep only the tiles whose
//! pixels actually differ from the last frame sent.
//!
//! The grid stores one XxHash64 per tile, not a pixel shadow: a 4K window uses
//! ~65 KB instead of ~33 MB. Hashes are compared only against the same tile
//! position, and the grid is reset on resize, so tile coordinates are stable.

use std::hash::Hasher;

use twox_hash::XxHash64;
use xcom_codec::Rect;

/// Tile edge in pixels. 32 gives a cursor cell 1–2 tiles and keeps the hash
/// grid small (a 4K window has ~8k tiles).
pub const TILE: u16 = 32;

/// Per-window grid of the last-sent tile hashes.
///
/// `None` means "unknown": the tile must be sent next time it falls inside a
/// large damage frame. Small damage does not update hashes; it marks the tiles
/// it touched unknown instead, which is cheaper and always conservative.
pub struct TileGrid {
    w: u16,
    h: u16,
    cols: usize,
    rows: usize,
    hashes: Vec<Option<u64>>,
}

impl TileGrid {
    pub fn new(w: u16, h: u16) -> TileGrid {
        let cols = div_ceil(w, TILE);
        let rows = div_ceil(h, TILE);
        TileGrid {
            w,
            h,
            cols,
            rows,
            hashes: vec![None; cols * rows],
        }
    }

    pub fn tile_count(&self) -> usize {
        self.cols * self.rows
    }

    /// Mark every tile intersecting `rect` unknown. Used after a small damage
    /// frame was sent without a full-window diff.
    pub fn mark_unknown_rect(&mut self, rect: Rect) {
        let rect = rect.clamped(self.w, self.h);
        if rect.is_empty() {
            return;
        }
        let (c0, c1) = tile_span(rect.x, rect.w);
        let (r0, r1) = tile_span(rect.y, rect.h);
        for row in r0..=r1 {
            for col in c0..=c1 {
                self.hashes[row * self.cols + col] = None;
            }
        }
    }

    /// Compare a full-window BGRX buffer with the stored hashes.
    ///
    /// Returns the changed tile rects (uncoalesced). Hashes are updated for
    /// every tile, so a subsequent identical frame returns an empty vec.
    /// `force` returns every tile (used by full captures).
    pub fn diff(&mut self, pixels: &[u8], force: bool) -> Vec<Rect> {
        let stride = self.w as usize * 4;
        debug_assert_eq!(pixels.len(), stride * self.h as usize);
        if pixels.len() != stride * self.h as usize {
            // Never silently diff a buffer with the wrong stride.
            return self.all_tiles();
        }

        let mut changed = Vec::new();
        let mut tile_buf = vec![0u8; TILE as usize * TILE as usize * 4];
        for row in 0..self.rows {
            let y = row as u16 * TILE;
            let th = TILE.min(self.h - y);
            for col in 0..self.cols {
                let x = col as u16 * TILE;
                let tw = TILE.min(self.w - x);
                let hash = tile_hash(&mut tile_buf, pixels, stride, x, y, tw, th);
                let slot = &mut self.hashes[row * self.cols + col];
                let old = slot.replace(hash);
                if force || old != Some(hash) {
                    changed.push(Rect::new(x, y, tw, th));
                }
            }
        }
        changed
    }

    /// Refresh the whole grid from a full-window buffer without allocating
    /// changed rects.
    pub fn update_full(&mut self, pixels: &[u8]) {
        let stride = self.w as usize * 4;
        if pixels.len() != stride * self.h as usize {
            self.hashes.fill(None);
            return;
        }
        let mut tile_buf = vec![0u8; TILE as usize * TILE as usize * 4];
        for row in 0..self.rows {
            let y = row as u16 * TILE;
            let th = TILE.min(self.h - y);
            for col in 0..self.cols {
                let x = col as u16 * TILE;
                let tw = TILE.min(self.w - x);
                self.hashes[row * self.cols + col] =
                    Some(tile_hash(&mut tile_buf, pixels, stride, x, y, tw, th));
            }
        }
    }

    fn all_tiles(&self) -> Vec<Rect> {
        let mut all = Vec::with_capacity(self.tile_count());
        for row in 0..self.rows {
            let y = row as u16 * TILE;
            let th = TILE.min(self.h - y);
            for col in 0..self.cols {
                let x = col as u16 * TILE;
                let tw = TILE.min(self.w - x);
                all.push(Rect::new(x, y, tw, th));
            }
        }
        all
    }
}

fn div_ceil(value: u16, divisor: u16) -> usize {
    (value as usize).div_ceil(divisor as usize)
}

/// Inclusive tile index range covered by a 1-D span.
fn tile_span(start: u16, len: u16) -> (usize, usize) {
    let first = (start as usize) / TILE as usize;
    let last = (start as u32 + len as u32 - 1) as usize / TILE as usize;
    (first, last)
}

/// Hash one tile's visible 24-bit pixels.
///
/// X's fourth byte is undefined padding for depth-24, and the client forces
/// opaque output, so it must not participate: otherwise the padding could flip
/// and turn an unchanged tile into a "changed" one.
fn tile_hash(
    scratch: &mut [u8],
    pixels: &[u8],
    stride: usize,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
) -> u64 {
    let mut out = 0usize;
    for row in 0..h as usize {
        let start = (y as usize + row) * stride + x as usize * 4;
        let src = &pixels[start..start + w as usize * 4];
        for pixel in src.chunks_exact(4) {
            let value = u32::from_ne_bytes([pixel[0], pixel[1], pixel[2], pixel[3]]) & 0x00ff_ffff;
            scratch[out..out + 4].copy_from_slice(&value.to_ne_bytes());
            out += 4;
        }
    }
    let mut hasher = XxHash64::with_seed(0);
    hasher.write(&scratch[..out]);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(w: u16, h: u16) -> Vec<u8> {
        // Opaque-ish BGRA; the alpha byte is deliberately arbitrary.
        let mut buf = vec![0u8; w as usize * h as usize * 4];
        for (i, pixel) in buf.chunks_exact_mut(4).enumerate() {
            pixel[0] = (i & 0xff) as u8;
            pixel[1] = ((i >> 8) & 0xff) as u8;
            pixel[2] = ((i >> 16) & 0xff) as u8;
            pixel[3] = 0xff;
        }
        buf
    }

    fn set_pixel(buf: &mut [u8], w: u16, x: u16, y: u16, value: [u8; 4]) {
        let at = (y as usize * w as usize + x as usize) * 4;
        buf[at..at + 4].copy_from_slice(&value);
    }

    #[test]
    fn force_sends_all_and_then_plain_diff_is_quiet() {
        let mut grid = TileGrid::new(64, 64);
        let buf = buffer(64, 64);
        assert_eq!(grid.diff(&buf, true).len(), 4);
        assert!(grid.diff(&buf, false).is_empty());
        assert_eq!(grid.diff(&buf, true).len(), 4);
    }

    #[test]
    fn one_pixel_changes_one_tile() {
        let mut grid = TileGrid::new(64, 64);
        let mut buf = buffer(64, 64);
        grid.update_full(&buf);
        set_pixel(&mut buf, 64, 40, 40, [1, 2, 3, 0xff]);
        let changed = grid.diff(&buf, false);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0], Rect::new(32, 32, 32, 32));
    }

    #[test]
    fn alpha_padding_is_ignored() {
        let mut grid = TileGrid::new(32, 32);
        let mut buf = buffer(32, 32);
        grid.update_full(&buf);
        // Change only the undefined fourth byte.
        let at = (5 * 32 + 5) * 4;
        buf[at + 3] ^= 0x7f;
        assert!(grid.diff(&buf, false).is_empty());
    }

    #[test]
    fn edge_tiles_are_clamped() {
        let mut grid = TileGrid::new(50, 40);
        let tiles = grid.diff(&buffer(50, 40), true);
        assert_eq!(tiles.len(), 4);
        assert_eq!(tiles[3], Rect::new(32, 32, 18, 8));
    }

    #[test]
    fn unknown_tiles_are_resent() {
        let mut grid = TileGrid::new(64, 64);
        let buf = buffer(64, 64);
        grid.update_full(&buf);
        grid.mark_unknown_rect(Rect::new(1, 1, 1, 1));
        let changed = grid.diff(&buf, false);
        assert_eq!(changed, vec![Rect::new(0, 0, 32, 32)]);
    }

    #[test]
    fn every_tile_in_a_range_is_invalidated() {
        let mut grid = TileGrid::new(64, 64);
        let buf = buffer(64, 64);
        grid.update_full(&buf);
        grid.mark_unknown_rect(Rect::new(31, 31, 2, 2));
        let changed = grid.diff(&buf, false);
        assert_eq!(changed.len(), 4);
    }
}
