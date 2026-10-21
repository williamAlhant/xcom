//! Native windows for remote X windows (winit + softbuffer).
//!
//! Correctness rules that came out of the first end-to-end test:
//!
//! 1. The pixel store for a remote window exists as soon as *anything* is
//!    known about it (its first frame or its create message). Frames travel on
//!    the bulk channel and window events on the control channel — two TCP
//!    connections with no ordering between them — so a frame may arrive first
//!    and must never be dropped.
//! 2. A native window is always painted immediately on creation (black until
//!    real content arrives): an unpainted window shows undefined display
//!    server memory.
//! 3. Blits use the frame's own window size as stride, independent of the
//!    local window size; a mismatch (WM constraints, DPI rounding) letterboxes
//!    instead of corrupting rows.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use raw_window_handle::HasWindowHandle;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use xcom_codec::decompress;
use xcom_proto::{DamageFrame, Message};

use crate::keys::{keysym_for, keysym_for_physical};

/// Events from the network threads to the UI thread.
pub enum UserEvent {
    Open {
        id: u32,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        title: String,
        kind: u8,
        parent: u32,
        override_redirect: bool,
    },
    Geometry {
        id: u32,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
    },
    /// Hidden remotely (unmapped): hide the native window, keep its store.
    Unmap {
        id: u32,
    },
    /// Visible again after an unmap: show and re-sync.
    Map {
        id: u32,
    },
    /// Destroyed remotely: drop the window and its store.
    Close {
        id: u32,
    },
    Frame(DamageFrame),
    /// Scripted runs: leave the event loop.
    Quit,
    /// The channel died: never leave stale windows showing old content.
    ConnectionLost,
    /// Test aid: resize the first local window (stress for the resize loop).
    ResizeLocal { w: u16, h: u16 },
    /// Test aid: dump the store now (transient artifacts included).
    Dump,
    /// Test aid: close one attached window (per-window close semantics).
    CloseOne,
    /// Text read from the remote selection (server reply).
    ClipboardData { text: String },
}

type Target = softbuffer::Surface<Arc<Window>, Arc<Window>>;

/// Pixel store for one remote window (remote-sized, BGRA as u32 0x00RRGGBB).
#[derive(Clone)]
struct Pixels {
    buf: Vec<u32>,
    w: u16,
    h: u16,
    title: String,
    x: i16,
    y: i16,
    /// Mini-WM metadata from `WindowCreate`.
    kind: u8,
    parent: u32,
    override_redirect: bool,
}

impl Pixels {
    fn new(w: u16, h: u16) -> Pixels {
        Pixels {
            buf: vec![0; w as usize * h as usize],
            w,
            h,
            title: String::new(),
            x: 0,
            y: 0,
            kind: xcom_proto::KIND_NORMAL,
            parent: 0,
            override_redirect: false,
        }
    }

    fn resize(&mut self, w: u16, h: u16) {
        if !sane_size(w, h) {
            return;
        }
        if (self.w, self.h) != (w, h) {
            // Keep the overlapping content: a resize clears nothing in the
            // remote window either, and wiping here made text vanish during
            // resize storms (the store was rebuilt from partial repaints).
            let mut buf = vec![0xff00_0000; w as usize * h as usize];
            let cw = (self.w.min(w)) as usize;
            let ch = (self.h.min(h)) as usize;
            for row in 0..ch {
                let dst = row * w as usize;
                let src = row * self.w as usize;
                buf[dst..dst + cw].copy_from_slice(&self.buf[src..src + cw]);
            }
            self.w = w;
            self.h = h;
            self.buf = buf;
        }
    }

    fn blit(&mut self, frame: &DamageFrame, decode_buf: &mut Vec<u8>) {
        self.resize(frame.win_w, frame.win_h);
        for rect in &frame.rects {
            let want = rect.w as usize * rect.h as usize * 4;
            if decompress(decode_buf, rect.codec, &rect.data, want).is_err() {
                continue;
            }
            let stride = self.w as usize;
            for row in 0..rect.h as usize {
                let dst_y = rect.y as usize + row;
                if dst_y >= self.h as usize {
                    break;
                }
                let dst_off = dst_y * stride + rect.x as usize;
                let src_off = row * rect.w as usize * 4;
                for col in 0..rect.w as usize {
                    if dst_off + col >= self.buf.len() {
                        break;
                    }
                    let b = &decode_buf[src_off + col * 4..src_off + col * 4 + 4];
                    // X's 4th byte is undefined padding for depth-24 windows;
                    // present it as opaque or compositors blend us into the
                    // desktop (Wayland ARGB).
                    self.buf[dst_off + col] =
                        u32::from_le_bytes(b.try_into().unwrap()) | 0xff00_0000;
                }
            }
        }
    }
}

/// A remote window with a native window attached.
struct Attached {
    window: Arc<Window>,
    target: Target,
    /// Local (surface) size in physical pixels.
    local: (u16, u16),
    /// Size the server last told us (to tell user resizes from echo resizes).
    remote: (u16, u16),
    cursor: (i16, i16),
    /// Override-redirect popups must not move the remote input focus.
    override_redirect: bool,
    /// Position we asked the compositor for (and the last known one). Wayland
    /// has no global positions, so parent-relative placement uses this.
    placed: (i32, i32),
}

pub struct App {
    input_tx: Sender<Message>,
    pixels: HashMap<u32, Pixels>,
    attached: HashMap<u32, Attached>,
    by_winit: HashMap<WindowId, u32>,
    decode_buf: Vec<u8>,
    frames_blitted: u64,
    presents: u64,
    present_errors: u64,
    /// A window has been opened at least once: used to distinguish "the last
    /// window was closed" from "nothing has arrived yet".
    ever_had_windows: bool,
    /// Keysym sent for each physical key currently down. The release event has
    /// no text and may carry a Wine-mangled logical key, so the only reliable
    /// way to release the same key the press created is to remember it here.
    held: HashMap<winit::keyboard::PhysicalKey, u32>,
    /// Plain (unmodified) character per physical key, learned from text events.
    /// Needed for Ctrl/Alt shortcuts under Wine, where winit reports a bogus
    /// named key and no text while a modifier is held.
    plain_chars: HashMap<winit::keyboard::PhysicalKey, u32>,
    /// Local (client) clipboard. None when the platform has no text clipboard.
    clipboard: Option<arboard::Clipboard>,
    dump: Option<PathBuf>,
    dump_seq: u32,
    fullscreen: bool,
    /// Test aids (F12 store dump) are opt-in: F12 belongs to the app otherwise.
    debug_tools: bool,
}

/// Refuse absurd window sizes from the wire: the store is `w*h*4` bytes, and
/// `u16` allows a 17 GB allocation from a malicious or buggy server.
const MAX_WINDOW_DIM: u16 = 8192;

fn sane_size(w: u16, h: u16) -> bool {
    w > 0 && h > 0 && w <= MAX_WINDOW_DIM && h <= MAX_WINDOW_DIM
}

/// Local and remote sizes within this many pixels are "the same size": window
/// managers round, toolkits snap to their character grid, and forwarding those
/// few pixels back as resizes starts an endless resize ping-pong.
const SIZE_TOLERANCE: u16 = 8;

fn size_close(a: (u16, u16), b: (u16, u16)) -> bool {
    a.0.abs_diff(b.0) <= SIZE_TOLERANCE && a.1.abs_diff(b.1) <= SIZE_TOLERANCE
}

impl App {
    pub fn new(
        _proxy: EventLoopProxy<UserEvent>,
        input_tx: Sender<Message>,
        dump: Option<PathBuf>,
        fullscreen: bool,
        debug_tools: bool,
    ) -> App {
        let clipboard = match arboard::Clipboard::new() {
            Ok(c) => Some(c),
            Err(e) => {
                eprintln!("[ui] clipboard unavailable: {e}");
                None
            }
        };
        App {
            input_tx,
            pixels: HashMap::new(),
            attached: HashMap::new(),
            by_winit: HashMap::new(),
            decode_buf: Vec::new(),
            frames_blitted: 0,
            presents: 0,
            present_errors: 0,
            ever_had_windows: false,
            held: HashMap::new(),
            plain_chars: HashMap::new(),
            clipboard,
            dump,
            dump_seq: 0,
            fullscreen,
            debug_tools,
        }
    }

    fn report(&mut self) {
        eprintln!(
            "[ui] {} window(s) open, {} frames blitted",
            self.attached.len(),
            self.frames_blitted
        );
        eprintln!(
            "[ui] presents={}, present_errors={}",
            self.presents, self.present_errors
        );
        self.dump("");
    }

    /// Write the largest window's pixel store as a PPM (test aid).
    fn dump(&mut self, suffix: &str) {
        let Some(base) = self.dump.clone() else {
            return;
        };
        let mut path = base.clone();
        if !suffix.is_empty() {
            self.dump_seq += 1;
            path = base.with_extension(format!("{suffix}{}ppm", self.dump_seq));
        }
        // Largest window: dump the pixel store, then read the *actual* window
        // contents back (X11) and compare — this is what catches presentation
        // bugs that frame counts and store dumps cannot.
        let Some((&id, _)) = self
            .attached
            .iter()
            .max_by_key(|(id, _)| self.pixels.get(id).map(|p| p.w as usize * p.h as usize).unwrap_or(0))
        else {
            eprintln!("[ui] no attached window to dump");
            return;
        };
        let pix = &self.pixels[&id];
        // Alpha bytes come from X's undefined padding for depth-24 windows and
        // matter on Wayland (ARGB); histogram them to see what we are shipping.
        let mut alphas = std::collections::HashMap::<u8, u64>::new();
        for px in &pix.buf {
            *alphas.entry((px >> 24) as u8).or_default() += 1;
        }
        let mut alphas: Vec<(u8, u64)> = alphas.into_iter().collect();
        alphas.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        eprintln!("[ui] store alpha histogram: {:#04x?}", &alphas[..alphas.len().min(6)]);
        let store = ppm_bytes(pix.w, pix.h, &pix.buf);
        match std::fs::write(&path, &store) {
            Ok(()) => eprintln!("[ui] dumped store {}x{} to {}", pix.w, pix.h, path.display()),
            Err(e) => eprintln!("[ui] cannot dump {}: {e}", path.display()),
        }

        let att = self.attached.get_mut(&id).unwrap();
        match att.target.fetch() {
            Ok(screen) => {
                let screen_path = path.with_extension("screen.ppm");
                let spix = Pixels {
                    buf: screen.clone(),
                    w: att.local.0,
                    h: att.local.1,
                    title: String::new(),
                    x: 0,
                    y: 0,
                    kind: xcom_proto::KIND_NORMAL,
                    parent: 0,
                    override_redirect: false,
                };
                let _ = std::fs::write(&screen_path, ppm_bytes(spix.w, spix.h, &spix.buf));
                // Compare RGB only: readback may set an alpha byte our store
                // does not have (0xff000000 vs 0x00000000 for the same black).
                let mut same = screen.len() == pix.buf.len();
                if same {
                    for (i, (a, b)) in screen.iter().zip(pix.buf.iter()).enumerate() {
                        if a & 0x00ff_ffff != b & 0x00ff_ffff {
                            eprintln!("[ui] first diff at px {i}: screen={a:#010x} store={b:#010x}");
                            same = false;
                            break;
                        }
                    }
                }
                eprintln!(
                    "[ui] screen readback {}x{}: {} vs store ({}x{}) -> {}",
                    att.local.0,
                    att.local.1,
                    screen.len(),
                    pix.w,
                    pix.h,
                    if same { "MATCH" } else { "MISMATCH" }
                );
            }
            Err(e) => eprintln!("[ui] screen readback unavailable: {e}"),
        }
    }

    /// Where a window should live locally: dialogs asking for (0,0) are
    /// centered on their parent, dialogs/popups keep their offset from the
    /// parent, normal windows keep the +32 screen offset.
    fn target_position(&self, id: u32, w: u16, h: u16) -> (i32, i32) {
        let Some(pix) = self.pixels.get(&id) else {
            return (32, 32);
        };
        let mut x = (pix.x as i32).max(0) + 32;
        let mut y = (pix.y as i32).max(0) + 32;
        if let Some(att) = self.attached.get(&pix.parent) {
            // Wayland has no global positions; fall back to what we asked for.
            let pp = att
                .window
                .outer_position()
                .map(|p| (p.x, p.y))
                .unwrap_or(att.placed);
            if pix.kind == xcom_proto::KIND_DIALOG && pix.x == 0 && pix.y == 0 {
                x = pp.0 + (att.local.0 as i32 - w as i32) / 2;
                y = pp.1 + (att.local.1 as i32 - h as i32) / 2;
            } else if pix.override_redirect
                || pix.kind == xcom_proto::KIND_MENU
                || pix.kind == xcom_proto::KIND_TOOLTIP
                || pix.kind == xcom_proto::KIND_DIALOG
            {
                if let Some(ppix) = self.pixels.get(&pix.parent) {
                    x = pp.0 + pix.x as i32 - ppix.x as i32;
                    y = pp.1 + pix.y as i32 - ppix.y as i32;
                }
            }
        }
        (x, y)
    }

    fn open_window(&mut self, el: &ActiveEventLoop, id: u32) {
        if self.attached.contains_key(&id) {
            return;
        }
        // The pixel store must exist by now (create message or first frame);
        // create it from the create message if the frame hasn't arrived yet.
        let Some(pix) = self.pixels.get(&id) else {
            return;
        };
        // Popups/menus must keep their real size: forcing 64x64 would draw
        // black margins (or a black box for tiny helper windows). The minimum
        // only exists to satisfy WM constraints for normal windows.
        let (w, h) = if pix.override_redirect {
            (pix.w.max(1), pix.h.max(1))
        } else {
            (pix.w.max(64), pix.h.max(64))
        };
        let title = if pix.title.is_empty() {
            format!("xcom {id:#x}")
        } else {
            format!("{} — xcom", pix.title)
        };
        let (kind, parent, override_redirect) = (pix.kind, pix.parent, pix.override_redirect);
        let (x, y) = self.target_position(id, w, h);
        let placed = (x, y);
        let attrs = Window::default_attributes()
            .with_title(title)
            // Menus/tooltips are chrome-less; dialogs keep decorations.
            .with_decorations(!override_redirect)
            .with_resizable(!override_redirect)
            .with_fullscreen(if self.fullscreen {
                Some(winit::window::Fullscreen::Borderless(None))
            } else {
                None
            })
            // 1:1 pixel mapping for now; DPI-aware scaling lands with M3.
            .with_inner_size(PhysicalSize::new(w, h))
            .with_position(winit::dpi::PhysicalPosition::new(x, y));
        // On X11, make remote override-redirect windows override-redirect
        // locally too: the WM then cannot move/tile/decorate them, so the
        // position is exact under any WM (Wayland ignores this field).
        #[cfg(target_os = "linux")]
        let attrs = {
            use winit::platform::x11::WindowAttributesExtX11;
            attrs.with_override_redirect(override_redirect)
        };
        let Ok(window) = el.create_window(attrs) else {
            return;
        };
        let window = Arc::new(window);
        let Ok(context) = softbuffer::Context::new(Arc::clone(&window)) else {
            return;
        };
        let mut target = match softbuffer::Surface::new(&context, Arc::clone(&window)) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("[ui] window {id:#x}: surface creation failed: {e}");
                return;
            }
        };
        if let Err(e) = target.resize(
            NonZeroU32::new(w as u32).unwrap(),
            NonZeroU32::new(h as u32).unwrap(),
        ) {
            eprintln!("[ui] window {id:#x}: surface resize {w}x{h} failed: {e}");
        }
        eprintln!(
            "[ui] window {id:#x} opened {w}x{h} at ({x},{y}) kind={kind} parent={parent:#x} override={override_redirect} (backend: {:?})",
            window
                .window_handle()
                .map(|h| format!("{:?}", h.as_raw()))
                .unwrap_or_else(|e| format!("unknown: {e}"))
        );
        // Dialogs should come to the front; popups are raised too, but their
        // Focused event does not move the remote X input focus.
        if override_redirect || kind == xcom_proto::KIND_DIALOG || kind == xcom_proto::KIND_MENU {
            window.focus_window();
        }
        self.by_winit.insert(window.id(), id);
        self.attached.insert(
            id,
            Attached {
                window,
                target,
                local: (w, h),
                remote: (pix.w, pix.h),
                cursor: (0, 0),
                override_redirect,
                placed,
            },
        );
        // Paint immediately (black + whatever content already arrived) and ask
        // the server for a full refresh: a new native window must never show
        // undefined display server memory.
        self.present(id);
        self.send(Message::FullRequest { id });
    }

    fn present(&mut self, id: u32) {
        let (Some(att), Some(pix)) = (self.attached.get_mut(&id), self.pixels.get(&id)) else {
            return;
        };
        let mut buf = match att.target.buffer_mut() {
            Ok(b) => b,
            Err(e) => {
                self.present_errors += 1;
                if self.present_errors <= 3 {
                    eprintln!(
                        "[ui] present {id:#x}: buffer_mut failed: {e} (local {:?}, remote {}x{})",
                        att.local, pix.w, pix.h
                    );
                }
                return;
            }
        };
        self.presents += 1;
        if self.presents <= 3 {
            eprintln!(
                "[ui] present #{}, win {id:#x}: buffer={}px local={:?} remote={}x{}",
                self.presents,
                buf.len(),
                att.local,
                pix.w,
                pix.h
            );
        }
        // Letterbox: rows of the remote image into the local surface. With the
        // current 1:1 mapping this is an exact copy except after WM/DPI
        // rounding; M3 adds scaling.
        buf.fill(0xff00_0000); // opaque black, never transparent
        let lw = att.local.0 as usize;
        let (rw, rh) = (pix.w as usize, pix.h as usize);
        if lw > 0 {
            let copy_w = lw.min(rw);
            let copy_h = (buf.len() / lw).min(rh);
            for row in 0..copy_h {
                let dst = row * lw..row * lw + copy_w;
                let src = row * rw..row * rw + copy_w;
                buf[dst].copy_from_slice(&pix.buf[src]);
            }
        }
        if let Err(e) = buf.present() {
            self.present_errors += 1;
            if self.present_errors <= 3 {
                eprintln!("[ui] present {id:#x}: present failed: {e}");
            }
        }
    }

    fn blit_frame(&mut self, frame: &DamageFrame) {
        let id = frame.window;
        if !sane_size(frame.win_w, frame.win_h) {
            eprintln!(
                "[ui] frame {id:#x} rejected: implausible window size {}x{}",
                frame.win_w, frame.win_h
            );
            return;
        }
        if xcom_proto::debug() {
            eprintln!(
                "[ui] frame {id:#x} {}x{} rects={:?}",
                frame.win_w,
                frame.win_h,
                frame.rects.iter().map(|r| (r.x, r.y, r.w, r.h)).collect::<Vec<_>>()
            );
        }
        // Never drop a frame: create the store if this is the first sign of the
        // window (its WindowCreate may still be in flight on the control channel).
        let pix = self
            .pixels
            .entry(id)
            .or_insert_with(|| Pixels::new(frame.win_w, frame.win_h));
        pix.blit(frame, &mut self.decode_buf);
        self.frames_blitted += 1;
        self.present(id);
    }

    fn send(&self, msg: Message) {
        self.input_tx.send(msg).ok();
    }

    fn remote_id(&self, id: WindowId) -> Option<u32> {
        self.by_winit.get(&id).copied()
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, _el: &ActiveEventLoop) {}

    fn user_event(&mut self, el: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Open {
                id,
                x,
                y,
                w,
                h,
                title,
                kind,
                parent,
                override_redirect,
            } => {
                if !sane_size(w, h) {
                    eprintln!("[ui] window {id:#x} rejected: implausible size {w}x{h}");
                    return;
                }
                self.ever_had_windows = true;
                let pix = self.pixels.entry(id).or_insert_with(|| Pixels::new(w, h));
                pix.x = x;
                pix.y = y;
                pix.title = title;
                pix.kind = kind;
                pix.parent = parent;
                pix.override_redirect = override_redirect;
                // If the size came from a frame first, trust the create message.
                pix.resize(w, h);
                if self.attached.contains_key(&id) {
                    // Re-announced window (unmap/remap raced with create).
                    if let Some(att) = self.attached.get(&id) {
                        att.window.set_visible(true);
                    }
                    self.present(id);
                    self.send(Message::FullRequest { id });
                } else {
                    self.open_window(el, id);
                }
            }
            UserEvent::Geometry { id, x, y, w, h } => {
                if xcom_proto::debug() {
                    eprintln!("[ui] geometry {id:#x} -> {w}x{h} at ({x},{y})");
                }
                if let Some(pix) = self.pixels.get_mut(&id) {
                    pix.x = x;
                    pix.y = y;
                    if sane_size(w, h) {
                        pix.resize(w, h);
                    }
                }
                if let Some(att) = self.attached.get_mut(&id) {
                    att.remote = (w, h);
                    // Follow the app's size, but only when it really differs:
                    // asking the WM to round-trip a 1px change loops forever.
                    if !size_close(att.local, att.remote) {
                        let (mw, mh) = if att.override_redirect {
                            (w.max(1), h.max(1))
                        } else {
                            (w.max(64), h.max(64))
                        };
                        let _ = att
                            .window
                            .request_inner_size(PhysicalSize::new(mw, mh));
                    }
                }
                // Toolkits map popups first and move them into place right
                // after; follow moves for dialogs/popups (normal windows are
                // left where the user put them).
                let is_popup = self
                    .pixels
                    .get(&id)
                    .map(|p| {
                        p.override_redirect
                            || p.kind == xcom_proto::KIND_DIALOG
                            || p.kind == xcom_proto::KIND_MENU
                            || p.kind == xcom_proto::KIND_TOOLTIP
                    })
                    .unwrap_or(false);
                if is_popup {
                    let (lw, lh) = self
                        .pixels
                        .get(&id)
                        .map(|p| {
                            if p.override_redirect {
                                (p.w.max(1), p.h.max(1))
                            } else {
                                (p.w.max(64), p.h.max(64))
                            }
                        })
                        .unwrap_or((w, h));
                    let (tx, ty) = self.target_position(id, lw, lh);
                    if let Some(att) = self.attached.get_mut(&id) {
                        if att.placed != (tx, ty) {
                            att.window.set_outer_position(
                                winit::dpi::PhysicalPosition::new(tx, ty),
                            );
                            att.placed = (tx, ty);
                            eprintln!("[ui] moved {id:#x} -> local ({tx},{ty})");
                        }
                    }
                }
                self.present(id);
            }
            UserEvent::Unmap { id } => {
                if let Some(att) = self.attached.get(&id) {
                    eprintln!("[ui] window {id:#x} unmapped (hidden, store kept)");
                    att.window.set_visible(false);
                }
            }
            UserEvent::Map { id } => {
                if let Some(att) = self.attached.get(&id) {
                    eprintln!("[ui] window {id:#x} mapped again");
                    att.window.set_visible(true);
                    self.present(id);
                    self.send(Message::FullRequest { id });
                } else if self.pixels.contains_key(&id) {
                    self.open_window(el, id);
                }
            }
            UserEvent::Close { id } => {
                self.pixels.remove(&id);
                if let Some(att) = self.attached.remove(&id) {
                    self.by_winit.remove(&att.window.id());
                    eprintln!("[ui] window {id:#x} closed");
                }
                if self.ever_had_windows && self.attached.is_empty() {
                    // The last remote window is gone: the session has nothing
                    // left to show.
                    self.report();
                    self.send(Message::Bye);
                    el.exit();
                }
            }
            UserEvent::Frame(frame) => self.blit_frame(&frame),
            UserEvent::Quit => {
                self.report();
                self.send(Message::Bye);
                el.exit();
            }
            UserEvent::ClipboardData { text } => {
                if text.is_empty() {
                    eprintln!("[ui] clipboard: remote selection is empty");
                } else if let Some(cb) = self.clipboard.as_mut() {
                    match cb.set_text(text.clone()) {
                        Ok(()) => {
                            eprintln!("[ui] clipboard: copied {} bytes locally", text.len())
                        }
                        Err(e) => eprintln!("[ui] clipboard write failed: {e}"),
                    }
                }
            }
            UserEvent::Dump => self.dump("mid."),
            UserEvent::CloseOne => {
                if let Some((&id, _)) = self.attached.iter().next() {
                    eprintln!("[ui] test aid: closing window {id:#x}");
                    self.send(Message::WindowClose { id });
                }
            }
            UserEvent::ResizeLocal { w, h } => {
                if let Some((id, att)) = self.attached.iter_mut().next() {
                    eprintln!("[ui] test resize {id:#x} -> {w}x{h}");
                    let _ = att.window.request_inner_size(PhysicalSize::new(w, h));
                }
            }
            UserEvent::ConnectionLost => {
                eprintln!("[ui] connection lost, closing {} window(s)", self.attached.len());
                self.report();
                el.exit();
            }
        }
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(remote) = self.remote_id(id) else {
            return;
        };
        match event {
            WindowEvent::Moved(pos) => {
                if let Some(att) = self.attached.get_mut(&remote) {
                    att.placed = (pos.x, pos.y);
                }
            }
            WindowEvent::CloseRequested => {
                // Close just this remote window (WM_DELETE_REQUEST); the
                // session ends when the last one is destroyed.
                eprintln!("[ui] close requested for win {remote:#x}");
                self.send(Message::WindowClose { id: remote });
            }
            WindowEvent::RedrawRequested => self.present(remote),
            WindowEvent::Resized(size) => {
                if xcom_proto::debug() {
                    eprintln!("[ui] resized {:?} (win {remote:#x})", size);
                }
                let mut reflow = None;
                if let Some(att) = self.attached.get_mut(&remote) {
                    att.local = (size.width as u16, size.height as u16);
                    let _ = att.target.resize(
                        NonZeroU32::new(size.width.max(1)).unwrap(),
                        NonZeroU32::new(size.height.max(1)).unwrap(),
                    );
                    if !size_close(att.local, att.remote) {
                        // The user resized us: ask the app to reflow. Small
                        // deltas are WM/toolkit rounding, not user actions.
                        att.remote = att.local;
                        reflow = Some(Message::Resize {
                            id: remote,
                            w: att.local.0,
                            h: att.local.1,
                        });
                    }
                }
                self.present(remote);
                if let Some(msg) = reflow {
                    self.send(msg);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                // The presented image is 1:1 at (0,0); anything beyond the
                // remote window is letterbox. Clamp, or a click in the band
                // lands outside the app on the remote root.
                let Some(pix) = self.pixels.get(&remote) else {
                    return;
                };
                let x = (position.x.round() as i32).clamp(0, pix.w.max(1) as i32 - 1) as i16;
                let y = (position.y.round() as i32).clamp(0, pix.h.max(1) as i32 - 1) as i16;
                if xcom_proto::debug() {
                    eprintln!("[ui] motion ({x},{y}) local={:?}", {
                        let a = self.attached.get(&remote);
                        a.map(|a| a.local)
                    });
                }
                if let Some(att) = self.attached.get_mut(&remote) {
                    att.cursor = (x, y);
                }
                self.send(Message::InputMotion {
                    window: remote,
                    x,
                    y,
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let (x, y) = self
                    .attached
                    .get(&remote)
                    .map(|a| a.cursor)
                    .unwrap_or((0, 0));
                if xcom_proto::debug() {
                    eprintln!("[ui] button {button:?} {state:?} at ({x},{y})");
                }
                self.send(Message::InputButton {
                    window: remote,
                    button: x11_button(button),
                    pressed: state == ElementState::Pressed,
                    x,
                    y,
                });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let dy = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p) => p.y as f32,
                };
                if dy.abs() < f32::EPSILON {
                    return;
                }
                let (x, y) = self
                    .attached
                    .get(&remote)
                    .map(|a| a.cursor)
                    .unwrap_or((0, 0));
                // X core wheel buttons: 4 = up, 5 = down.
                let button = if dy > 0.0 { 4 } else { 5 };
                for pressed in [true, false] {
                    self.send(Message::InputButton {
                        window: remote,
                        button,
                        pressed,
                        x,
                        y,
                    });
                }
            }
            WindowEvent::Focused(focused) => {
                if xcom_proto::debug() {
                    eprintln!("[ui] focused={focused} (win {remote:#x})");
                }
                if !focused {
                    // Releases may never arrive after losing OS focus; never
                    // leave a remote key held (the X server would autorepeat it).
                    let held: Vec<u32> = self.held.drain().map(|(_, sym)| sym).collect();
                    for sym in held {
                        self.send(Message::InputKey {
                            sym,
                            pressed: false,
                        });
                    }
                    return;
                }
                // Popups rely on the app's own grabs; moving the remote input
                // focus to a menu breaks its keyboard handling.
                if self
                    .attached
                    .get(&remote)
                    .map(|a| a.override_redirect)
                    .unwrap_or(false)
                {
                    return;
                }
                // Keyboard focus follows OS window focus: without a WM on the
                // remote display, XTEST keystrokes would go to whatever the
                // remote pointer happens to be over.
                self.send(Message::Focus { window: remote });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                // The remote X server autorepeats a held XTEST key by itself;
                // forwarding the local repeats too would double the rate.
                if event.repeat {
                    return;
                }
                if event.state == ElementState::Released {
                    // Release exactly what the press sent: the release has no
                    // text and Wine may report a bogus named key. Keys whose
                    // press sent nothing are not in the map, so they stay silent.
                    if let Some(sym) = self.held.remove(&event.physical_key) {
                        self.send(Message::InputKey {
                            sym,
                            pressed: false,
                        });
                    }
                    return;
                }

                const CTRL: [u32; 2] = [0xffe3, 0xffe4];
                const ALT: [u32; 2] = [0xffe9, 0xffea];
                const SUPER: [u32; 2] = [0xffeb, 0xffec];
                const SHIFT: [u32; 2] = [0xffe1, 0xffe2];
                let held_any = |syms: [u32; 2]| {
                    self.held
                        .values()
                        .any(|s| syms.contains(s))
                };
                let ctrl = held_any(CTRL);
                let shift = held_any(SHIFT);
                let alt_super = held_any(ALT) || held_any(SUPER);
                let ctrl_alt = ctrl || alt_super;

                // Local clipboard shortcuts (not forwarded): copy the remote
                // selection locally, or push the local clipboard remotely.
                if ctrl && shift && !alt_super {
                    match event.physical_key {
                        winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyC) => {
                            eprintln!("[ui] clipboard: requesting the remote selection");
                            self.send(Message::ClipboardGet {
                                target: xcom_proto::CLIPBOARD_CLIPBOARD,
                            });
                            return;
                        }
                        winit::keyboard::PhysicalKey::Code(winit::keyboard::KeyCode::KeyV) => {
                            match self.clipboard.as_mut().map(|cb| cb.get_text()) {
                                Some(Ok(text)) if !text.is_empty() => {
                                    let text = xcom_proto::truncate_clipboard(text);
                                    eprintln!(
                                        "[ui] clipboard: pushing {} bytes to the remote",
                                        text.len()
                                    );
                                    self.send(Message::ClipboardSet {
                                        target: xcom_proto::CLIPBOARD_CLIPBOARD,
                                        text,
                                    });
                                }
                                Some(Ok(_)) => eprintln!("[ui] clipboard: local clipboard is empty"),
                                Some(Err(e)) => eprintln!("[ui] clipboard read failed: {e}"),
                                None => eprintln!("[ui] clipboard: no local clipboard"),
                            }
                            return;
                        }
                        _ => {}
                    }
                }

                let text_sym = crate::keys::text_keysym(event.text.as_deref());
                // Learn the layout's plain character. ASCII only and with no
                // modifiers, so dead-key accents and shifted symbols can't
                // pollute the map.
                if !ctrl_alt && !shift {
                    if let Some(sym) = text_sym {
                        if (0x20..=0x7e).contains(&sym) {
                            self.plain_chars.insert(event.physical_key, sym);
                        }
                    }
                }

                // Local hotkey (test aid): dump the store exactly when the user
                // sees the artifact (not forwarded to the remote app). Opt-in.
                if self.debug_tools
                    && matches!(event.logical_key, winit::keyboard::Key::Named(
                        winit::keyboard::NamedKey::F12
                    ))
                {
                    eprintln!("[ui] F12: dumping store and requesting full refresh");
                    self.dump("mid.");
                    if let Some(&id) = self.attached.keys().next() {
                        self.send(Message::FullRequest { id });
                    }
                    return;
                }

                let logical_sym = keysym_for(&event.logical_key);
                let sym = if ctrl_alt && text_sym.is_none() {
                    if matches!(event.logical_key, winit::keyboard::Key::Character(_)) {
                        // Real Windows: the logical key is the layout character
                        // even with Ctrl held.
                        logical_sym
                    } else {
                        match event.physical_key {
                            winit::keyboard::PhysicalKey::Code(code)
                                if crate::keys::is_text_key(code) =>
                            {
                                // Wine: bogus named VK. Use the character this
                                // key normally produces; send nothing if the
                                // user has not typed it yet (no wrong guesses).
                                self.plain_chars.get(&event.physical_key).copied()
                            }
                            _ => logical_sym.or_else(|| match event.physical_key {
                                winit::keyboard::PhysicalKey::Code(code) => {
                                    keysym_for_physical(code)
                                }
                                _ => None,
                            }),
                        }
                    }
                } else {
                    text_sym
                        .or(logical_sym)
                        .or_else(|| match event.physical_key {
                            winit::keyboard::PhysicalKey::Code(code) => {
                                keysym_for_physical(code)
                            }
                            _ => None,
                        })
                };
                if xcom_proto::debug_keys() {
                    eprintln!(
                        "[ui] key {:?} -> sym={} pressed={} text={:?}",
                        event.logical_key,
                        sym.map(|s| format!("{s:#x}"))
                            .unwrap_or_else(|| "NONE".into()),
                        event.state == ElementState::Pressed,
                        event.text
                    );
                }
                if let Some(sym) = sym {
                    self.held.insert(event.physical_key, sym);
                    self.send(Message::InputKey {
                        sym,
                        pressed: true,
                    });
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, _el: &ActiveEventLoop) {}
}

fn x11_button(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 3,
        MouseButton::Back => 8,
        MouseButton::Forward => 9,
        MouseButton::Other(n) => n as u8,
    }
}

/// Encode 0x00RRGGBB pixels as a binary PPM (P6).
fn ppm_bytes(w: u16, h: u16, buf: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(buf.len() * 3 + 32);
    out.extend_from_slice(format!("P6\n{} {}\n255\n", w, h).as_bytes());
    for px in buf {
        out.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, *px as u8]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_preserves_content() {
        let mut p = Pixels::new(4, 3);
        for (i, px) in p.buf.iter_mut().enumerate() {
            *px = 0xff00_0000 + i as u32;
        }
        p.resize(6, 5);
        // grown: old content kept at its positions, new area opaque black
        assert_eq!(p.buf[0], 0xff00_0000);
        assert_eq!(p.buf[3], 0xff00_0000 + 3);
        assert_eq!(p.buf[6], 0xff00_0000 + 4); // row 1 shifted to new stride
        assert_eq!(p.buf[7], 0xff00_0000 + 5);
        assert_eq!(p.buf[4], 0xff00_0000); // new column area
        p.resize(2, 2);
        assert_eq!(p.buf[0], 0xff00_0000);
        assert_eq!(p.buf[1], 0xff00_0000 + 1);
        assert_eq!(p.buf[2], 0xff00_0000 + 4); // row 1 of the old content
        assert_eq!(p.buf[3], 0xff00_0000 + 5);
    }

    #[test]
    fn implausible_sizes_are_rejected() {
        assert!(sane_size(800, 600));
        assert!(!sane_size(0, 600));
        assert!(!sane_size(800, 0));
        assert!(!sane_size(65535, 65535));
        assert!(!sane_size(8193, 100));
    }

    #[test]
    fn rounding_is_not_a_user_resize() {
        // WM/toolkit rounding and character-grid snapping must not be
        // forwarded as resizes (that started the ping-pong).
        assert!(size_close((484, 316), (484, 316)));
        assert!(size_close((492, 320), (493, 322)));
        assert!(!size_close((484, 316), (520, 340)));
    }
}
