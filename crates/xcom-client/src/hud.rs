//! Metrics HUD: a dependency-free overlay drawn straight into the softbuffer.
//!
//! Glyphs come from the public-domain 8x8 bitmap font by Daniel Hepper
//! (`font8x8`, based on the public-domain IBM VGA fonts), embedded so the
//! shipping client keeps its "no runtime dependencies" property. Each glyph is
//! 8 bytes, one per pixel row, bit 0 = leftmost pixel.

/// 8x8 glyphs for ASCII `0x20..=0x7e`, row-major.
#[rustfmt::skip]
const FONT: [[u8; 8]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],  // ' '
    [0x18, 0x3c, 0x3c, 0x18, 0x18, 0x00, 0x18, 0x00],  // '!'
    [0x36, 0x36, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],  // '"'
    [0x36, 0x36, 0x7f, 0x36, 0x7f, 0x36, 0x36, 0x00],  // '#'
    [0x0c, 0x3e, 0x03, 0x1e, 0x30, 0x1f, 0x0c, 0x00],  // '$'
    [0x00, 0x63, 0x33, 0x18, 0x0c, 0x66, 0x63, 0x00],  // '%'
    [0x1c, 0x36, 0x1c, 0x6e, 0x3b, 0x33, 0x6e, 0x00],  // '&'
    [0x06, 0x06, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00],  // "'"
    [0x18, 0x0c, 0x06, 0x06, 0x06, 0x0c, 0x18, 0x00],  // '('
    [0x06, 0x0c, 0x18, 0x18, 0x18, 0x0c, 0x06, 0x00],  // ')'
    [0x00, 0x66, 0x3c, 0xff, 0x3c, 0x66, 0x00, 0x00],  // '*'
    [0x00, 0x0c, 0x0c, 0x3f, 0x0c, 0x0c, 0x00, 0x00],  // '+'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x0c, 0x06],  // ','
    [0x00, 0x00, 0x00, 0x3f, 0x00, 0x00, 0x00, 0x00],  // '-'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x0c, 0x0c, 0x00],  // '.'
    [0x60, 0x30, 0x18, 0x0c, 0x06, 0x03, 0x01, 0x00],  // '/'
    [0x3e, 0x63, 0x73, 0x7b, 0x6f, 0x67, 0x3e, 0x00],  // '0'
    [0x0c, 0x0e, 0x0c, 0x0c, 0x0c, 0x0c, 0x3f, 0x00],  // '1'
    [0x1e, 0x33, 0x30, 0x1c, 0x06, 0x33, 0x3f, 0x00],  // '2'
    [0x1e, 0x33, 0x30, 0x1c, 0x30, 0x33, 0x1e, 0x00],  // '3'
    [0x38, 0x3c, 0x36, 0x33, 0x7f, 0x30, 0x78, 0x00],  // '4'
    [0x3f, 0x03, 0x1f, 0x30, 0x30, 0x33, 0x1e, 0x00],  // '5'
    [0x1c, 0x06, 0x03, 0x1f, 0x33, 0x33, 0x1e, 0x00],  // '6'
    [0x3f, 0x33, 0x30, 0x18, 0x0c, 0x0c, 0x0c, 0x00],  // '7'
    [0x1e, 0x33, 0x33, 0x1e, 0x33, 0x33, 0x1e, 0x00],  // '8'
    [0x1e, 0x33, 0x33, 0x3e, 0x30, 0x18, 0x0e, 0x00],  // '9'
    [0x00, 0x0c, 0x0c, 0x00, 0x00, 0x0c, 0x0c, 0x00],  // ':'
    [0x00, 0x0c, 0x0c, 0x00, 0x00, 0x0c, 0x0c, 0x06],  // ';'
    [0x18, 0x0c, 0x06, 0x03, 0x06, 0x0c, 0x18, 0x00],  // '<'
    [0x00, 0x00, 0x3f, 0x00, 0x00, 0x3f, 0x00, 0x00],  // '='
    [0x06, 0x0c, 0x18, 0x30, 0x18, 0x0c, 0x06, 0x00],  // '>'
    [0x1e, 0x33, 0x30, 0x18, 0x0c, 0x00, 0x0c, 0x00],  // '?'
    [0x3e, 0x63, 0x7b, 0x7b, 0x7b, 0x03, 0x1e, 0x00],  // '@'
    [0x0c, 0x1e, 0x33, 0x33, 0x3f, 0x33, 0x33, 0x00],  // 'A'
    [0x3f, 0x66, 0x66, 0x3e, 0x66, 0x66, 0x3f, 0x00],  // 'B'
    [0x3c, 0x66, 0x03, 0x03, 0x03, 0x66, 0x3c, 0x00],  // 'C'
    [0x1f, 0x36, 0x66, 0x66, 0x66, 0x36, 0x1f, 0x00],  // 'D'
    [0x7f, 0x46, 0x16, 0x1e, 0x16, 0x46, 0x7f, 0x00],  // 'E'
    [0x7f, 0x46, 0x16, 0x1e, 0x16, 0x06, 0x0f, 0x00],  // 'F'
    [0x3c, 0x66, 0x03, 0x03, 0x73, 0x66, 0x7c, 0x00],  // 'G'
    [0x33, 0x33, 0x33, 0x3f, 0x33, 0x33, 0x33, 0x00],  // 'H'
    [0x1e, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x1e, 0x00],  // 'I'
    [0x78, 0x30, 0x30, 0x30, 0x33, 0x33, 0x1e, 0x00],  // 'J'
    [0x67, 0x66, 0x36, 0x1e, 0x36, 0x66, 0x67, 0x00],  // 'K'
    [0x0f, 0x06, 0x06, 0x06, 0x46, 0x66, 0x7f, 0x00],  // 'L'
    [0x63, 0x77, 0x7f, 0x7f, 0x6b, 0x63, 0x63, 0x00],  // 'M'
    [0x63, 0x67, 0x6f, 0x7b, 0x73, 0x63, 0x63, 0x00],  // 'N'
    [0x1c, 0x36, 0x63, 0x63, 0x63, 0x36, 0x1c, 0x00],  // 'O'
    [0x3f, 0x66, 0x66, 0x3e, 0x06, 0x06, 0x0f, 0x00],  // 'P'
    [0x1e, 0x33, 0x33, 0x33, 0x3b, 0x1e, 0x38, 0x00],  // 'Q'
    [0x3f, 0x66, 0x66, 0x3e, 0x36, 0x66, 0x67, 0x00],  // 'R'
    [0x1e, 0x33, 0x07, 0x0e, 0x38, 0x33, 0x1e, 0x00],  // 'S'
    [0x3f, 0x2d, 0x0c, 0x0c, 0x0c, 0x0c, 0x1e, 0x00],  // 'T'
    [0x33, 0x33, 0x33, 0x33, 0x33, 0x33, 0x3f, 0x00],  // 'U'
    [0x33, 0x33, 0x33, 0x33, 0x33, 0x1e, 0x0c, 0x00],  // 'V'
    [0x63, 0x63, 0x63, 0x6b, 0x7f, 0x77, 0x63, 0x00],  // 'W'
    [0x63, 0x63, 0x36, 0x1c, 0x1c, 0x36, 0x63, 0x00],  // 'X'
    [0x33, 0x33, 0x33, 0x1e, 0x0c, 0x0c, 0x1e, 0x00],  // 'Y'
    [0x7f, 0x63, 0x31, 0x18, 0x4c, 0x66, 0x7f, 0x00],  // 'Z'
    [0x1e, 0x06, 0x06, 0x06, 0x06, 0x06, 0x1e, 0x00],  // '['
    [0x03, 0x06, 0x0c, 0x18, 0x30, 0x60, 0x40, 0x00],  // '\\'
    [0x1e, 0x18, 0x18, 0x18, 0x18, 0x18, 0x1e, 0x00],  // ']'
    [0x08, 0x1c, 0x36, 0x63, 0x00, 0x00, 0x00, 0x00],  // '^'
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff],  // '_'
    [0x0c, 0x0c, 0x18, 0x00, 0x00, 0x00, 0x00, 0x00],  // '`'
    [0x00, 0x00, 0x1e, 0x30, 0x3e, 0x33, 0x6e, 0x00],  // 'a'
    [0x07, 0x06, 0x06, 0x3e, 0x66, 0x66, 0x3b, 0x00],  // 'b'
    [0x00, 0x00, 0x1e, 0x33, 0x03, 0x33, 0x1e, 0x00],  // 'c'
    [0x38, 0x30, 0x30, 0x3e, 0x33, 0x33, 0x6e, 0x00],  // 'd'
    [0x00, 0x00, 0x1e, 0x33, 0x3f, 0x03, 0x1e, 0x00],  // 'e'
    [0x1c, 0x36, 0x06, 0x0f, 0x06, 0x06, 0x0f, 0x00],  // 'f'
    [0x00, 0x00, 0x6e, 0x33, 0x33, 0x3e, 0x30, 0x1f],  // 'g'
    [0x07, 0x06, 0x36, 0x6e, 0x66, 0x66, 0x67, 0x00],  // 'h'
    [0x0c, 0x00, 0x0e, 0x0c, 0x0c, 0x0c, 0x1e, 0x00],  // 'i'
    [0x30, 0x00, 0x30, 0x30, 0x30, 0x33, 0x33, 0x1e],  // 'j'
    [0x07, 0x06, 0x66, 0x36, 0x1e, 0x36, 0x67, 0x00],  // 'k'
    [0x0e, 0x0c, 0x0c, 0x0c, 0x0c, 0x0c, 0x1e, 0x00],  // 'l'
    [0x00, 0x00, 0x33, 0x7f, 0x7f, 0x6b, 0x63, 0x00],  // 'm'
    [0x00, 0x00, 0x1f, 0x33, 0x33, 0x33, 0x33, 0x00],  // 'n'
    [0x00, 0x00, 0x1e, 0x33, 0x33, 0x33, 0x1e, 0x00],  // 'o'
    [0x00, 0x00, 0x3b, 0x66, 0x66, 0x3e, 0x06, 0x0f],  // 'p'
    [0x00, 0x00, 0x6e, 0x33, 0x33, 0x3e, 0x30, 0x78],  // 'q'
    [0x00, 0x00, 0x3b, 0x6e, 0x66, 0x06, 0x0f, 0x00],  // 'r'
    [0x00, 0x00, 0x3e, 0x03, 0x1e, 0x30, 0x1f, 0x00],  // 's'
    [0x08, 0x0c, 0x3e, 0x0c, 0x0c, 0x2c, 0x18, 0x00],  // 't'
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x33, 0x6e, 0x00],  // 'u'
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x1e, 0x0c, 0x00],  // 'v'
    [0x00, 0x00, 0x63, 0x6b, 0x7f, 0x7f, 0x36, 0x00],  // 'w'
    [0x00, 0x00, 0x63, 0x36, 0x1c, 0x36, 0x63, 0x00],  // 'x'
    [0x00, 0x00, 0x33, 0x33, 0x33, 0x3e, 0x30, 0x1f],  // 'y'
    [0x00, 0x00, 0x3f, 0x19, 0x0c, 0x26, 0x3f, 0x00],  // 'z'
    [0x38, 0x0c, 0x0c, 0x07, 0x0c, 0x0c, 0x38, 0x00],  // '{'
    [0x18, 0x18, 0x18, 0x00, 0x18, 0x18, 0x18, 0x00],  // '|'
    [0x07, 0x0c, 0x0c, 0x38, 0x0c, 0x0c, 0x07, 0x00],  // '}'
    [0x6e, 0x3b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],  // '~'
];

const GLYPH_W: usize = 8;
/// One text line: the font's last row is empty, so 8 is already spaced.
const LINE_H: usize = 9;
/// Vertical space reserved for the sparkline (including a 2px gap).
const GRAPH_H: usize = 22;

const PANEL: u32 = 0x0000_0000;
/// Opaque enough to read over any content, including a black terminal.
const PANEL_ALPHA: u32 = 230;
/// Colours carry an explicit opaque alpha: the overlay lives in an ARGB
/// surface, so an alpha-0 pixel is transparent on Wayland/Windows (only X11's
/// 24-bit visual ignores the byte) and the text disappears.
const FG: u32 = 0xffe6e6e6;
const DIM: u32 = 0xffa0a0a0;
const GRAPH_FG: u32 = 0xff60c060;

/// Text lines plus a bandwidth history for the metrics overlay.
#[derive(Default, Clone)]
pub struct Hud {
    /// Lines of text, drawn top to bottom.
    pub lines: Vec<String>,
    /// Bytes/s samples (oldest first) for the sparkline.
    pub graph: Vec<u32>,
}

/// Alpha-blend `src` over `dst` (`a`: 0 keeps `dst`, 255 takes `src`).
fn mix(dst: u32, src: u32, a: u32) -> u32 {
    let ch = |s: u32, d: u32| ((s * a + d * (255 - a)) / 255) & 0xff;
    let r = ch((src >> 16) & 0xff, (dst >> 16) & 0xff);
    let g = ch((src >> 8) & 0xff, (dst >> 8) & 0xff);
    let b = ch(src & 0xff, dst & 0xff);
    0xff00_0000 | (r << 16) | (g << 8) | b
}

/// Glyph for an ASCII character, or `None` for anything outside the table.
fn glyph(c: char) -> Option<&'static [u8; 8]> {
    let i = (c as u32).checked_sub(0x20)? as usize;
    FONT.get(i)
}

/// Draw `s` at `(x, y)` in `color`, clipped to the buffer.
fn text(buf: &mut [u32], w: usize, h: usize, x: usize, y: usize, s: &str, color: u32) {
    let mut cx = x;
    for c in s.chars() {
        if let Some(g) = glyph(c) {
            for (row, bits) in g.iter().enumerate() {
                let py = y + row;
                if py >= h {
                    break;
                }
                for col in 0..GLYPH_W {
                    if bits & (1 << col) == 0 {
                        continue;
                    }
                    let px = cx + col;
                    if px < w {
                        buf[py * w + px] = color | 0xff00_0000;
                    }
                }
            }
        }
        cx += GLYPH_W;
    }
}

/// Draw a bar sparkline of `samples` in a `size` box at `at`.
fn graph(
    buf: &mut [u32],
    buf_w: usize,
    buf_h: usize,
    at: (usize, usize),
    size: (usize, usize),
    samples: &[u32],
) {
    let (x, y) = at;
    let (gw, gh) = size;
    if samples.is_empty() || gw == 0 || gh == 0 {
        return;
    }
    let max = samples.iter().copied().max().unwrap_or(0).max(1);
    let n = samples.len().min(gw).max(1);
    let bar_w = (gw / n).max(1);
    for (i, &s) in samples[samples.len() - n..].iter().enumerate() {
        let bh = (s as u64 * gh as u64 / max as u64) as usize;
        for row in 0..bh {
            let py = y + gh - 1 - row;
            if py >= buf_h {
                continue;
            }
            for col in 0..bar_w {
                let px = x + i * bar_w + col;
                if px < buf_w {
                    buf[py * buf_w + px] = GRAPH_FG | 0xff00_0000;
                }
            }
        }
    }
}

/// Overlay the panel, text lines and sparkline into a `w`x`h` BGRA buffer.
pub fn draw(buf: &mut [u32], w: usize, h: usize, hud: &Hud) {
    if w == 0 || h == 0 || buf.len() < w * h {
        return;
    }
    if hud.lines.is_empty() && hud.graph.is_empty() {
        return;
    }
    let pad = 4usize;
    let graph_h = if hud.graph.is_empty() { 0 } else { GRAPH_H };
    let text_w = hud
        .lines
        .iter()
        .map(|l| l.chars().count() * GLYPH_W)
        .max()
        .unwrap_or(0);
    let panel_w = (text_w + pad * 2).min(w);
    let panel_h = (pad * 2 + LINE_H * hud.lines.len() + graph_h).min(h);
    // Translucent panel: the HUD must stay readable over any content.
    for y in 0..panel_h {
        for x in 0..panel_w {
            buf[y * w + x] = mix(buf[y * w + x], PANEL, PANEL_ALPHA);
        }
    }
    let mut y = pad;
    for (i, line) in hud.lines.iter().enumerate() {
        text(buf, w, h, pad, y, line, if i == 0 { DIM } else { FG });
        y += LINE_H;
    }
    if graph_h > 0 {
        graph(
            buf,
            w,
            h,
            (pad, y),
            (panel_w.saturating_sub(pad * 2), graph_h - 2),
            &hud.graph,
        );
    }
}

/// Buttons of the last-window close dialog, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseChoice {
    /// Close the remote window; the app (and the session) ends.
    Close,
    /// Leave the app running and detach this client.
    Disconnect,
    /// Dismiss the dialog.
    Cancel,
}

const CLOSE_CHOICES: [(CloseChoice, &str); 3] = [
    (CloseChoice::Close, "Close window"),
    (CloseChoice::Disconnect, "Disconnect"),
    (CloseChoice::Cancel, "Cancel"),
];

#[derive(Clone, Copy)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Rect {
    fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64
            && y >= self.y as f64
            && x < (self.x + self.w) as f64
            && y < (self.y + self.h) as f64
    }
}

/// Panel and button rectangles for a `w`x`h` surface. Kept in one place so
/// drawing and hit-testing cannot drift apart.
fn close_dialog_layout(w: usize, h: usize) -> (Rect, [(CloseChoice, Rect); 3]) {
    let panel_w = w.saturating_sub(20).clamp(1, 460);
    let panel_h = h.saturating_sub(20).min(132);
    let panel = Rect {
        x: (w - panel_w) / 2,
        y: (h - panel_h) / 2,
        w: panel_w,
        h: panel_h,
    };
    let gap = 8usize;
    let btn_w = (panel_w.saturating_sub(gap * 4) / 3).max(1);
    let btn_h = 26.min(panel_h);
    let by = panel.y + panel_h.saturating_sub(btn_h + 12);
    let mut buttons = [(CloseChoice::Close, Rect { x: 0, y: 0, w: 0, h: 0 }); 3];
    for (i, (choice, _)) in CLOSE_CHOICES.iter().enumerate() {
        buttons[i] = (
            *choice,
            Rect {
                x: panel.x + gap + i * (btn_w + gap),
                y: by,
                w: btn_w,
                h: btn_h,
            },
        );
    }
    (panel, buttons)
}

fn fill_rect(buf: &mut [u32], w: usize, h: usize, r: Rect, color: u32, alpha: u32) {
    for y in r.y..(r.y + r.h).min(h) {
        for x in r.x..(r.x + r.w).min(w) {
            let i = y * w + x;
            buf[i] = mix(buf[i], color, alpha);
        }
    }
}

fn stroke_rect(buf: &mut [u32], w: usize, h: usize, r: Rect, color: u32) {
    if r.w == 0 || r.h == 0 {
        return;
    }
    fill_rect(buf, w, h, Rect { x: r.x, y: r.y, w: r.w, h: 1 }, color, 255);
    if r.h > 1 {
        fill_rect(
            buf,
            w,
            h,
            Rect { x: r.x, y: r.y + r.h - 1, w: r.w, h: 1 },
            color,
            255,
        );
    }
    fill_rect(buf, w, h, Rect { x: r.x, y: r.y, w: 1, h: r.h }, color, 255);
    if r.w > 1 {
        fill_rect(
            buf,
            w,
            h,
            Rect { x: r.x + r.w - 1, y: r.y, w: 1, h: r.h },
            color,
            255,
        );
    }
}

/// Draw the "close the last window?" dialog over a presented surface.
///
/// `focus` is the button activated by Enter; `hover` highlights the button
/// under the pointer. Colors carry opaque alpha for Wayland/Windows surfaces.
pub fn draw_close_dialog(
    buf: &mut [u32],
    w: usize,
    h: usize,
    focus: CloseChoice,
    hover: Option<CloseChoice>,
) {
    if w == 0 || h == 0 || buf.len() < w * h {
        return;
    }
    let (panel, buttons) = close_dialog_layout(w, h);
    fill_rect(buf, w, h, panel, 0x0000_0000, 236);
    stroke_rect(buf, w, h, panel, 0x0080_8080);
    text(
        buf,
        w,
        h,
        panel.x + 14,
        panel.y + 12,
        "Close the last window?",
        FG,
    );
    text(
        buf,
        w,
        h,
        panel.x + 14,
        panel.y + 30,
        "The session ends when the app quits.",
        DIM,
    );
    for (choice, rect) in buttons {
        let (color, alpha) = if choice == focus {
            (0x003a_6ea5, 255) // focused: blue
        } else if hover == Some(choice) {
            (0x0048_4848, 255)
        } else {
            (0x0024_2424, 255)
        };
        fill_rect(buf, w, h, rect, color, alpha);
        stroke_rect(buf, w, h, rect, if choice == focus { FG } else { DIM });
        let label = CLOSE_CHOICES
            .iter()
            .find(|(c, _)| *c == choice)
            .map(|(_, l)| *l)
            .unwrap_or("");
        let tw = label.chars().count() * GLYPH_W;
        let tx = rect.x + rect.w.saturating_sub(tw) / 2;
        let ty = rect.y + rect.h.saturating_sub(8) / 2;
        text(buf, w, h, tx, ty, label, FG);
    }
}

/// Which dialog button (if any) is at surface position `(x, y)`.
///
/// `None` means "not a button" (outside the panel or on its background), which
/// the caller treats as Cancel: click-outside dismisses the dialog.
pub fn close_dialog_hit(w: usize, h: usize, x: f64, y: f64) -> Option<CloseChoice> {
    let (panel, buttons) = close_dialog_layout(w, h);
    if !panel.contains(x, y) {
        return None;
    }
    buttons
        .iter()
        .find(|(_, rect)| rect.contains(x, y))
        .map(|(choice, _)| *choice)
}

/// Next/previous button for Tab and the arrow keys.
pub fn close_choice_step(current: CloseChoice, forward: bool) -> CloseChoice {
    let idx = CLOSE_CHOICES
        .iter()
        .position(|(choice, _)| *choice == current)
        .unwrap_or(1);
    let n = CLOSE_CHOICES.len();
    let next = if forward {
        (idx + 1) % n
    } else {
        (idx + n - 1) % n
    };
    CLOSE_CHOICES[next].0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_buttons_cycle_and_hit() {
        assert_eq!(CloseChoice::Disconnect, close_choice_step(CloseChoice::Close, true));
        assert_eq!(CloseChoice::Cancel, close_choice_step(CloseChoice::Disconnect, true));
        assert_eq!(CloseChoice::Close, close_choice_step(CloseChoice::Cancel, true));
        assert_eq!(CloseChoice::Cancel, close_choice_step(CloseChoice::Close, false));

        let (panel, buttons) = close_dialog_layout(800, 600);
        let (_, middle) = buttons[1];
        let x = (middle.x + middle.w / 2) as f64;
        let y = (middle.y + middle.h / 2) as f64;
        assert_eq!(close_dialog_hit(800, 600, x, y), Some(CloseChoice::Disconnect));
        assert_eq!(close_dialog_hit(800, 600, 1.0, 1.0), None);
        assert!(panel.contains(x, y));
    }

    #[test]
    fn font_covers_printable_ascii() {
        assert!(glyph(' ').is_some());
        assert!(glyph('~').is_some());
        assert!(glyph('\u{7f}').is_none());
        assert!(glyph('\u{80}').is_none());
        // Spot-check a glyph against the source table ('A').
        assert_eq!(glyph('A').unwrap(), &[0x0c, 0x1e, 0x33, 0x33, 0x3f, 0x33, 0x33, 0x00]);
    }

    #[test]
    fn text_sets_glyph_pixels() {
        let (w, h) = (16usize, 16usize);
        let mut buf = vec![0xff00_0000u32; w * h];
        text(&mut buf, w, h, 0, 0, "A", FG);
        assert_eq!(buf[0], 0xff00_0000, "row 0 bit 0 must stay clear");
        assert_eq!(buf[2], FG, "row 0 bit 2 sets a pixel");
        assert_eq!(buf[2 * w + 1], FG, "row 2 bit 1 sets a pixel");
        // Clipping must neither wrap nor panic.
        let mut tiny = vec![0u32; 4];
        text(&mut tiny, 2, 2, 0, 0, "WWWW", FG);
    }

    #[test]
    fn draw_paints_panel_text_and_graph() {
        let (w, h) = (48usize, 32usize);
        let mut buf = vec![0xffff_ffffu32; w * h];
        let hud = Hud {
            lines: vec!["xcom".into(), "fps 60".into()],
            graph: vec![1, 5, 3],
        };
        draw(&mut buf, w, h, &hud);
        assert!(buf[0] != 0xffff_ffff, "panel darkened the corner");
        assert!(buf.contains(&FG), "text pixels drawn");
        assert!(buf.contains(&GRAPH_FG), "graph pixels drawn");
        // Degenerate buffers must be safe.
        let mut one = vec![0u32; 1];
        draw(&mut one, 1, 1, &hud);
        let mut short = vec![0u32; 3];
        draw(&mut short, 4, 4, &hud);
    }

    #[test]
    fn every_written_pixel_is_opaque() {
        // The overlay is composited in an ARGB surface (Wayland, Windows): an
        // alpha-0 pixel there is invisible/half-blended, which is exactly the
        // "glitchy unreadable HUD" bug. X11's 24-bit visual hides it, so this
        // must be asserted directly, not only through the X11 e2e readback.
        let (w, h) = (200usize, 80usize);
        let mut buf = vec![0xff00_0000u32; w * h];
        let hud = Hud {
            lines: vec![
                "xcom v6  rtt 3ms  win 2".into(),
                "fps 60.0/60.0  rx 1.2MB/s".into(),
                "x228.6  cap 136us  enc 10us  rects 63".into(),
                "in->px 8.2ms  pres 17883".into(),
            ],
            graph: vec![1, 5, 3, 9, 2, 7],
        };
        draw(&mut buf, w, h, &hud);
        let bad: Vec<(usize, u32)> = buf
            .iter()
            .copied()
            .enumerate()
            .filter(|(_, p)| (p >> 24) as u8 != 0xff)
            .take(4)
            .collect();
        assert!(bad.is_empty(), "non-opaque pixels written: {bad:?}");
        // Text and graph must actually be visible (not the panel only).
        assert!(buf.contains(&FG), "label pixels missing");
        assert!(buf.contains(&GRAPH_FG), "graph pixels missing");
    }

    #[test]
    fn empty_hud_draws_nothing() {
        let (w, h) = (8usize, 8usize);
        let mut buf = vec![0xff11_2233u32; w * h];
        draw(&mut buf, w, h, &Hud::default());
        assert!(buf.iter().all(|&p| p == 0xff11_2233));
    }
}
