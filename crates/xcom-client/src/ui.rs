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
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Instant;

use raw_window_handle::HasWindowHandle;
use winit::application::ApplicationHandler;
use winit::dpi::PhysicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::keyboard::ModifiersState;
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use xcom_codec::decompress;
use xcom_proto::{DamageFrame, Message};
use xcom_proto::logln;

use crate::keys::{KeyAction, KeyState};
use crate::metrics::{Metrics, ServerStats};
use crate::hud::CloseChoice;

/// What closing the last attached window should do.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum CloseAction {
    /// Ask with an in-window dialog (default).
    Ask,
    /// Close the remote window (the app quits and the session ends).
    Close,
    /// Detach and leave the session running.
    Disconnect,
}

/// Startup options for the UI (kept separate to keep `App::new` readable).
pub struct AppOptions {
    pub dump: Option<PathBuf>,
    pub fullscreen: bool,
    pub debug_tools: bool,
    pub hud: bool,
    pub queue_us: Arc<AtomicU32>,
    pub clipboard_sync: bool,
    /// What a request to close the last window does (dialog by default).
    pub close_action: CloseAction,
    /// Largest local monitor, for the "session is smaller than this monitor"
    /// note. `None` when it could not be queried.
    pub monitor: Option<(u16, u16)>,
}

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
    /// Session display size (first control message). Windows are capped at it.
    Display {
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
    /// The channel died. With `reconnect`, the window set is frozen under a
    /// banner while the supervisor re-establishes the link.
    ConnectionLost {
        reconnect: bool,
    },
    /// The session is ending on the server (the app exited or it was killed).
    /// Do not reconnect.
    SessionEnded,
    /// A reconnect attempt is about to start (attempt numbers start at 1).
    Reconnecting {
        attempt: u32,
    },
    /// A fresh link is up; the server resends the window set and frames.
    Reconnected,
    /// A reconnect attempt failed but will be retried.
    ReconnectFailed(String),
    /// The session itself is gone: no retry can succeed.
    SessionGone(String),
    /// Another client is attached and the busy retries ran out.
    SessionBusy(String),
    /// The reattach initial sync is complete; windows not re-announced are gone.
    SyncDone,
    /// Test aid: resize the first local window (stress for the resize loop).
    ResizeLocal { w: u16, h: u16 },
    /// Test aid: dump the store now (transient artifacts included).
    Dump,
    /// Test aid: close one attached window (per-window close semantics).
    CloseOne,
    /// Text read from the remote selection (server reply).
    ClipboardData { text: String },
    /// Remote pointer image (ARGB premultiplied).
    Cursor {
        xhot: u16,
        yhot: u16,
        w: u16,
        h: u16,
        pixels: Vec<u32>,
    },
    /// Server-side metrics for the last interval.
    Stats(ServerStats),
    /// Ping round-trip time measured on the control channel.
    Rtt {
        ms: u32,
    },
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
            // Opaque black, like `resize`: an unpainted window must never be
            // transparent (Wayland/Windows composite the alpha byte).
            buf: vec![0xff00_0000; w as usize * h as usize],
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
    /// Hidden by an unmap (the store is kept).
    visible: bool,
    /// Position we asked the compositor for (and the last known one). Wayland
    /// has no global positions, so parent-relative placement uses this.
    placed: (i32, i32),
}

pub struct App {
    input_tx: crate::link::InputSink,
    pixels: HashMap<u32, Pixels>,
    attached: HashMap<u32, Attached>,
    by_winit: HashMap<WindowId, u32>,
    decode_buf: Vec<u8>,
    frames_blitted: u64,
    present_errors: u64,
    /// A window has been opened at least once: used to distinguish "the last
    /// window was closed" from "nothing has arrived yet".
    ever_had_windows: bool,
    /// Keysym bookkeeping and the local key bindings (Ctrl+Shift+C/V/M).
    keys: KeyState,
    /// Local clipboard: applies remote pushes and mirrors local changes.
    clipboard: crate::clipboard::LocalClipboard,
    /// Cursor mirroring the remote pointer, applied to every window.
    custom_cursor: Option<winit::window::CustomCursor>,
    /// Numbered cursor dumps under XCOM_DEBUG.
    cursor_seq: u32,
    dump: Option<PathBuf>,
    dump_seq: u32,
    fullscreen: bool,
    /// Test aids (F12 store dump) are opt-in: F12 belongs to the app otherwise.
    debug_tools: bool,
    /// Rates, RTT, server counters and the HUD overlay.
    metrics: Metrics,
    /// Focus left the session with a popup open: dismiss the remote menu if
    /// nothing of ours regains focus by then.
    dismiss_at: Option<Instant>,
    /// Whether the local pointer is inside any of our windows. A menu must not
    /// be dismissed while the user is interacting with it.
    pointer_inside: bool,
    /// Modifier state from winit. The keysym map only knows about modifiers we
    /// saw a *press* for; Wayland reports modifiers held before (or while) the
    /// window got focus only through `ModifiersChanged`.
    mods: ModifiersState,
    /// Session display size. The X pointer cannot leave the remote screen, so
    /// a window larger than this would have a region that is visible but
    /// unclickable; windows are capped at it.
    screen: Option<(u16, u16)>,
    /// Largest local monitor, for the startup note.
    monitor: Option<(u16, u16)>,
    /// What a close request on the last window does.
    close_action: CloseAction,
    /// Modal "close the last window?" dialog, when one is up.
    dialog: Option<CloseDialog>,
    /// Reconnect status banner (none while the link is healthy).
    banner: Option<String>,
    /// Windows that existed before the current outage. On SyncDone, the ones
    /// the server did not re-announce are closed.
    stale: Option<std::collections::HashSet<u32>>,
    /// Windows re-announced by the new link since the outage.
    confirmed: std::collections::HashSet<u32>,
    /// The server said the session is ending; ignore later link events.
    ended: bool,
    /// The client asked the last window to close: a following link loss is the
    /// session ending, not a network problem to report or retry.
    closing_last: bool,
    /// Last local pointer position (physical surface pixels), for hit-testing
    /// the dialog (the remote cursor is clamped to the store and cannot be
    /// used for the letterboxed surface).
    pointer: (f64, f64),
}

struct CloseDialog {
    /// The window the dialog is drawn on (the last attached window).
    window: u32,
    focus: CloseChoice,
    hover: Option<CloseChoice>,
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

/// A window may not exceed the session display: the X pointer cannot leave the
/// remote screen, so the excess would be visible but unclickable.
fn cap_to_display(size: (u16, u16), display: Option<(u16, u16)>) -> (u16, u16) {
    match display {
        Some((dw, dh)) => (size.0.min(dw), size.1.min(dh)),
        None => size,
    }
}

/// (parent local position, parent local size, parent remote position).
type ParentPlacement = ((i32, i32), (u16, u16), (i16, i16));

/// Origin to anchor a child's content-relative position to.
///
/// Remote coordinates are content-relative. For an *undecorated* child (menus,
/// tooltips) the parent's decorations must be subtracted, so its client-area
/// origin (`inner`) is the anchor — otherwise the popup lands one title bar too
/// high. For a *decorated* child (dialogs) the parent's and the child's
/// decorations cancel out, so the outer origin is the right anchor there.
///
/// `inner`/`outer` are `None` where the platform does not report positions
/// (Wayland); `placed` is then the best available guess.
fn parent_origin(
    outer: Option<(i32, i32)>,
    inner: Option<(i32, i32)>,
    placed: (i32, i32),
    child_undecorated: bool,
) -> (i32, i32) {
    if child_undecorated {
        if let Some(p) = inner {
            return p;
        }
    }
    outer.unwrap_or(placed)
}

/// Pure window placement: normal windows keep the remote position plus a 32 px
/// cascade; dialogs that ask for (0,0) are centered on their parent;
/// dialogs/popups keep the same offset from the parent that the remote windows
/// have. `parent` is (local position, local size, remote position).
fn place_window(
    pix_x: i16,
    pix_y: i16,
    kind: u8,
    override_redirect: bool,
    w: u16,
    h: u16,
    parent: Option<ParentPlacement>,
) -> (i32, i32) {
    let mut x = (pix_x as i32).max(0) + 32;
    let mut y = (pix_y as i32).max(0) + 32;
    if let Some(((px, py), (lw, lh), (rx, ry))) = parent {
        if kind == xcom_proto::KIND_DIALOG && pix_x == 0 && pix_y == 0 {
            x = px + (lw as i32 - w as i32) / 2;
            y = py + (lh as i32 - h as i32) / 2;
        } else if override_redirect
            || kind == xcom_proto::KIND_MENU
            || kind == xcom_proto::KIND_TOOLTIP
            || kind == xcom_proto::KIND_DIALOG
        {
            x = px + pix_x as i32 - rx as i32;
            y = py + pix_y as i32 - ry as i32;
        }
    }
    (x, y)
}

impl App {
    pub fn new(
        _proxy: EventLoopProxy<UserEvent>,
        sink: crate::link::InputSink,
        opts: AppOptions,
    ) -> App {
        let AppOptions {
            dump,
            fullscreen,
            debug_tools,
            hud,
            queue_us,
            clipboard_sync,
            close_action,
            monitor,
        } = opts;
        let clipboard = crate::clipboard::spawn(sink.clone(), clipboard_sync);
        App {
            input_tx: sink,
            pixels: HashMap::new(),
            attached: HashMap::new(),
            by_winit: HashMap::new(),
            decode_buf: Vec::new(),
            frames_blitted: 0,
            present_errors: 0,
            ever_had_windows: false,
            keys: KeyState::new(),
            clipboard,
            custom_cursor: None,
            cursor_seq: 0,
            dump,
            dump_seq: 0,
            fullscreen,
            debug_tools,
            metrics: Metrics::new(hud, queue_us),
            dismiss_at: None,
            pointer_inside: false,
            mods: ModifiersState::empty(),
            screen: None,
            monitor,
            close_action,
            dialog: None,
            pointer: (0.0, 0.0),
            banner: None,
            stale: None,
            confirmed: std::collections::HashSet::new(),
            ended: false,
            closing_last: false,
        }
    }

    fn report(&mut self) {
        self.metrics.tick(true);
        logln!(
            "[ui] {} window(s) open, {} frames blitted",
            self.attached.len(),
            self.frames_blitted
        );
        logln!(
            "[ui] presents={}, present_errors={}",
            self.metrics.presents(),
            self.present_errors
        );
        logln!("[ui] metrics: {}", self.metrics.line());
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
            logln!("[ui] no attached window to dump");
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
        logln!("[ui] store alpha histogram: {:#04x?}", &alphas[..alphas.len().min(6)]);
        let store = ppm_bytes(pix.w, pix.h, &pix.buf);
        match std::fs::write(&path, &store) {
            Ok(()) => logln!("[ui] dumped store {}x{} to {}", pix.w, pix.h, path.display()),
            Err(e) => logln!("[ui] cannot dump {}: {e}", path.display()),
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
                            logln!("[ui] first diff at px {i}: screen={a:#010x} store={b:#010x}");
                            same = false;
                            break;
                        }
                    }
                }
                logln!(
                    "[ui] screen readback {}x{}: {} vs store ({}x{}) -> {}",
                    att.local.0,
                    att.local.1,
                    screen.len(),
                    pix.w,
                    pix.h,
                    if same { "MATCH" } else { "MISMATCH" }
                );
            }
            Err(e) => logln!("[ui] screen readback unavailable: {e}"),
        }
    }

    /// Where a window should live locally: dialogs asking for (0,0) are
    /// centered on their parent, dialogs/popups keep their offset from the
    /// parent, normal windows keep the +32 screen offset.
    fn target_position(&self, id: u32, w: u16, h: u16) -> (i32, i32) {
        let Some(pix) = self.pixels.get(&id) else {
            return (32, 32);
        };
        let parent = match (self.pixels.get(&pix.parent), self.attached.get(&pix.parent)) {
            (Some(ppix), Some(att)) => {
                // Wayland has no global positions: both fail and we fall back
                // to what we asked for.
                let outer = att.window.outer_position().map(|p| (p.x, p.y)).ok();
                let inner = att.window.inner_position().map(|p| (p.x, p.y)).ok();
                let origin = parent_origin(outer, inner, att.placed, pix.override_redirect);
                if xcom_proto::debug() {
                    logln!(
                        "[ui] place {id:#x} kind={} override={} parent={:#x} \
                         outer={outer:?} inner={inner:?} origin={origin:?}",
                        pix.kind,
                        pix.override_redirect,
                        pix.parent
                    );
                }
                Some((origin, att.local, (ppix.x, ppix.y)))
            }
            _ => None,
        };
        let pos = place_window(
            pix.x,
            pix.y,
            pix.kind,
            pix.override_redirect,
            w,
            h,
            parent,
        );
        if xcom_proto::debug() {
            logln!(
                "[ui] place {id:#x} remote=({},{}) -> local=({},{})",
                pix.x,
                pix.y,
                pos.0,
                pos.1
            );
        }
        pos
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
        if let Some((sw, sh)) = self.screen {
            window.set_max_inner_size(Some(PhysicalSize::new(sw as u32, sh as u32)));
        }
        // The remote display has no window manager, so nothing there gives the
        // window keyboard focus: without this, XTEST keys go to the root (and
        // a bare client X server never sends Focused(true)). Popups rely on
        // the app's own grabs, so they are left alone.
        if !override_redirect {
            self.send(Message::Focus { window: id });
        }
        if let Some(cursor) = &self.custom_cursor {
            window.set_cursor(cursor.clone());
        }
        let Ok(context) = softbuffer::Context::new(Arc::clone(&window)) else {
            return;
        };
        let mut target = match softbuffer::Surface::new(&context, Arc::clone(&window)) {
            Ok(t) => t,
            Err(e) => {
                logln!("[ui] window {id:#x}: surface creation failed: {e}");
                return;
            }
        };
        if let Err(e) = target.resize(
            NonZeroU32::new(w as u32).unwrap(),
            NonZeroU32::new(h as u32).unwrap(),
        ) {
            logln!("[ui] window {id:#x}: surface resize {w}x{h} failed: {e}");
        }
        logln!(
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
                visible: true,
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
        let dialog = self
            .dialog
            .as_ref()
            .filter(|d| d.window == id)
            .map(|d| (d.focus, d.hover));
        let banner = self.banner.clone();
        let (Some(att), Some(pix)) = (self.attached.get_mut(&id), self.pixels.get(&id)) else {
            return;
        };
        let mut buf = match att.target.buffer_mut() {
            Ok(b) => b,
            Err(e) => {
                self.present_errors += 1;
                if self.present_errors <= 3 {
                    logln!(
                        "[ui] present {id:#x}: buffer_mut failed: {e} (local {:?}, remote {}x{})",
                        att.local, pix.w, pix.h
                    );
                }
                return;
            }
        };
        self.metrics.on_present();
        if self.metrics.presents() <= 3 {
            logln!(
                "[ui] present #{}, win {id:#x}: buffer={}px local={:?} remote={}x{}",
                self.metrics.presents(),
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
        let lh = buf.len().checked_div(lw).unwrap_or(0);
        if lw > 0 {
            let copy_w = lw.min(rw);
            let copy_h = lh.min(rh);
            for row in 0..copy_h {
                let dst = row * lw..row * lw + copy_w;
                let src = row * rw..row * rw + copy_w;
                buf[dst].copy_from_slice(&pix.buf[src]);
            }
        }
        // The HUD is an overlay on the presented surface, never in the store:
        // it must not be mistaken for remote content.
        if self.metrics.hud_on() {
            crate::hud::draw(&mut buf, lw, lh, self.metrics.hud());
        }
        if let Some(text) = &banner {
            crate::hud::draw_banner(&mut buf, lw, lh, text);
        }
        if let Some((focus, hover)) = dialog {
            crate::hud::draw_close_dialog(&mut buf, lw, lh, focus, hover);
        }
        if let Err(e) = buf.present() {
            self.present_errors += 1;
            if self.present_errors <= 3 {
                logln!("[ui] present {id:#x}: present failed: {e}");
            }
        }
    }

    fn blit_frame(&mut self, frame: &DamageFrame) {
        let id = frame.window;
        self.confirmed.insert(id);
        if !sane_size(frame.win_w, frame.win_h) {
            logln!(
                "[ui] frame {id:#x} rejected: implausible window size {}x{}",
                frame.win_w, frame.win_h
            );
            return;
        }
        if xcom_proto::debug() {
            logln!(
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
        let raw = frame.rects.iter().map(|r| r.raw_len as u64).sum::<u64>();
        self.metrics
            .on_frame(id, frame.encoded_len() as u64, raw);
        self.metrics.tick(false);
        self.present(id);
    }

    fn send(&self, msg: Message) {
        // Enqueue with a timestamp: the writer thread measures the queue delay.
        self.input_tx.send(msg);
    }

    /// Send input and remember when, for the input -> present latency readout.
    fn send_input(&mut self, window: u32, msg: Message) {
        self.metrics.on_input(window);
        self.send(msg);
    }

    fn remote_id(&self, id: WindowId) -> Option<u32> {
        self.by_winit.get(&id).copied()
    }

    /// Request a repaint of every attached window (banner changes).
    fn redraw_all(&self) {
        for att in self.attached.values() {
            att.window.request_redraw();
        }
    }

    /// Show the modal "close the last window?" dialog on `window`.
    fn open_close_dialog(&mut self, window: u32) {
        self.dialog = Some(CloseDialog {
            window,
            focus: CloseChoice::Disconnect,
            hover: None,
        });
        self.present(window);
    }

    /// Activate the focused dialog button.
    fn activate_close_dialog(&mut self, el: &ActiveEventLoop, window: u32) {
        let Some(choice) = self.dialog.as_ref().map(|d| d.focus) else {
            return;
        };
        self.dialog = None;
        match choice {
            CloseChoice::Close => {
                logln!("[ui] close dialog: closing the window");
                self.closing_last = true;
                self.send(Message::WindowClose { id: window });
                self.present(window);
            }
            CloseChoice::Disconnect => {
                logln!("[ui] close dialog: disconnect (leaving the session running)");
                self.report();
                self.send(Message::Bye);
                el.exit();
            }
            CloseChoice::Cancel => {
                logln!("[ui] close dialog: cancelled");
                self.present(window);
            }
        }
    }

    /// Dismiss the dialog without acting (Esc or a click outside the buttons).
    fn cancel_close_dialog(&mut self, window: u32) {
        if self.dialog.take().is_some() {
            logln!("[ui] close dialog: cancelled");
            self.present(window);
        }
    }

    fn focus_close_dialog(&mut self, window: u32, forward: bool) {
        if let Some(dialog) = self.dialog.as_mut() {
            dialog.focus = crate::hud::close_choice_step(dialog.focus, forward);
            dialog.hover = None;
            self.present(window);
        }
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
                    logln!("[ui] window {id:#x} rejected: implausible size {w}x{h}");
                    return;
                }
                self.ever_had_windows = true;
                self.confirmed.insert(id);
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
                    logln!("[ui] geometry {id:#x} -> {w}x{h} at ({x},{y})");
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
                            logln!("[ui] moved {id:#x} -> local ({tx},{ty})");
                        }
                    }
                }
                self.present(id);
            }
            UserEvent::Unmap { id } => {
                if let Some(att) = self.attached.get_mut(&id) {
                    logln!("[ui] window {id:#x} unmapped (hidden, store kept)");
                    att.window.set_visible(false);
                    att.visible = false;
                }
                if self.dialog.as_ref().is_some_and(|d| d.window == id) {
                    self.dialog = None;
                }
            }
            UserEvent::Map { id } => {
                if let Some(att) = self.attached.get_mut(&id) {
                    logln!("[ui] window {id:#x} mapped again");
                    att.window.set_visible(true);
                    att.visible = true;
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
                    logln!("[ui] window {id:#x} closed");
                }
                if self.dialog.as_ref().is_some_and(|d| d.window == id) {
                    self.dialog = None;
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
                    logln!("[ui] clipboard: remote selection is empty");
                } else {
                    self.clipboard.apply(&text);
                    logln!("[ui] clipboard: mirrored {} bytes locally", text.len());
                }
            }
            UserEvent::Cursor {
                xhot,
                yhot,
                w,
                h,
                pixels,
            } => {
                if xcom_proto::debug() {
                    self.dump_cursor(w, h, xhot, yhot, &pixels);
                }
                let rgba = cursor_rgba(&pixels);
                let hx = xhot.min(w.saturating_sub(1));
                let hy = yhot.min(h.saturating_sub(1));
                match winit::window::CustomCursor::from_rgba(rgba, w, h, hx, hy) {
                    Ok(source) => {
                        let cursor = el.create_custom_cursor(source);
                        if xcom_proto::debug() {
                            logln!("[ui] cursor {w}x{h} hot=({hx},{hy})");
                        }
                        for att in self.attached.values() {
                            att.window.set_cursor(cursor.clone());
                        }
                        self.custom_cursor = Some(cursor);
                    }
                    Err(e) => logln!("[ui] cursor rejected: {e:?}"),
                }
            }
            UserEvent::Stats(s) => {
                self.metrics.on_stats(s);
                // A fresh interval arrived: refresh the readouts right away.
                if self.metrics.hud_on() {
                    self.metrics.tick(false);
                }
            }
            UserEvent::Rtt { ms } => self.metrics.on_rtt(ms),
            UserEvent::Dump => self.dump("mid."),
            UserEvent::CloseOne => {
                if let Some((&id, _)) = self.attached.iter().next() {
                    logln!("[ui] test aid: closing window {id:#x}");
                    self.send(Message::WindowClose { id });
                }
            }
            UserEvent::ResizeLocal { w, h } => {
                if let Some((id, att)) = self.attached.iter_mut().next() {
                    logln!("[ui] test resize {id:#x} -> {w}x{h}");
                    let _ = att.window.request_inner_size(PhysicalSize::new(w, h));
                }
            }
            UserEvent::Display { w, h } => {
                self.screen = Some((w, h));
                logln!("[xcom] session display {w}x{h}");
                if let Some((mw, mh)) = self.monitor {
                    if w < mw || h < mh {
                        logln!(
                            "[xcom] note: the session display ({w}x{h}) is smaller than this \
                             monitor ({mw}x{mh}); windows are capped at the session size. \
                             Pass --spawn {mw}x{mh} to request a larger display."
                        );
                    }
                }
                // Normally the first message, before any window exists; apply
                // to anything already open in case the order ever changes.
                for att in self.attached.values() {
                    att.window
                        .set_max_inner_size(Some(PhysicalSize::new(w as u32, h as u32)));
                }
            }
            UserEvent::ConnectionLost { reconnect } => {
                if self.ended {
                    return;
                }
                if reconnect && self.closing_last {
                    logln!("[ui] session ended after closing the last window");
                    self.ended = true;
                    self.report();
                    el.exit();
                    return;
                }
                if reconnect {
                    logln!(
                        "[ui] connection lost, keeping {} window(s) for reattach",
                        self.attached.len()
                    );
                    self.stale = Some(self.attached.keys().copied().collect());
                    self.confirmed.clear();
                    self.banner = Some("connection lost — reconnecting…".to_string());
                    self.redraw_all();
                } else {
                    self.dialog = None;
                    logln!("[ui] connection lost, closing {} window(s)", self.attached.len());
                    self.report();
                    el.exit();
                }
            }
            UserEvent::SessionEnded => {
                if self.ended {
                    return;
                }
                self.ended = true;
                logln!("[ui] session ended");
                self.report();
                el.exit();
            }
            UserEvent::Reconnecting { attempt } => {
                if self.ended {
                    return;
                }
                self.banner =
                    Some(format!("connection lost — reconnecting (attempt {attempt})…"));
                self.redraw_all();
            }
            UserEvent::Reconnected => {
                if self.ended {
                    return;
                }
                logln!("[ui] reconnected; waiting for the window resync");
                self.banner = Some("reconnected — resyncing…".to_string());
                self.redraw_all();
            }
            UserEvent::ReconnectFailed(error) => {
                if self.ended {
                    return;
                }
                logln!("[ui] reconnect failed: {error}");
                self.banner = Some(format!("reconnect failed: {error}"));
                self.redraw_all();
            }
            UserEvent::SessionGone(error) => {
                if self.ended {
                    return;
                }
                logln!("[ui] session is gone: {error}");
                self.report();
                el.exit();
            }
            UserEvent::SessionBusy(error) => {
                if self.ended {
                    return;
                }
                logln!("[ui] another client is attached: {error}");
                self.report();
                el.exit();
            }
            UserEvent::SyncDone => {
                if self.ended {
                    return;
                }
                self.banner = None;
                if let Some(stale) = self.stale.take() {
                    let gone: Vec<u32> = stale.difference(&self.confirmed).copied().collect();
                    for id in gone {
                        logln!("[ui] window {id:#x} disappeared while detached");
                        self.pixels.remove(&id);
                        if let Some(att) = self.attached.remove(&id) {
                            self.by_winit.remove(&att.window.id());
                        }
                    }
                }
                self.redraw_all();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        // Modifier state is global and must be tracked even for events that
        // arrive before/without a mapped remote window.
        if let WindowEvent::ModifiersChanged(m) = &event {
            self.mods = m.state();
            return;
        }
        let Some(remote) = self.remote_id(id) else {
            return;
        };
        match event {
            WindowEvent::CursorEntered { .. } => {
                if xcom_proto::debug() {
                    logln!("[ui] cursor entered {remote:#x}");
                }
                self.pointer_inside = true;
            }
            WindowEvent::CursorLeft { .. } => {
                if xcom_proto::debug() {
                    logln!("[ui] cursor left {remote:#x}");
                }
                self.pointer_inside = false;
            }
            WindowEvent::Moved(pos) => {
                if let Some(att) = self.attached.get_mut(&remote) {
                    att.placed = (pos.x, pos.y);
                }
            }
            WindowEvent::CloseRequested => {
                // Closing the last window is ambiguous now: close it (the app
                // quits and the session ends) or disconnect and leave it
                // running. Ask with an in-window dialog unless a test policy
                // says otherwise.
                if self.attached.len() > 1 {
                    logln!("[ui] close requested for win {remote:#x}");
                    self.send(Message::WindowClose { id: remote });
                } else {
                    match self.close_action {
                        CloseAction::Close => {
                            logln!("[ui] close requested for win {remote:#x}");
                            self.closing_last = true;
                            self.send(Message::WindowClose { id: remote });
                        }
                        CloseAction::Disconnect => {
                            logln!("[ui] close requested: disconnecting");
                            self.report();
                            self.send(Message::Bye);
                            el.exit();
                        }
                        CloseAction::Ask => {
                            logln!("[ui] close requested for the last window: asking");
                            self.open_close_dialog(remote);
                        }
                    }
                }
            }
            WindowEvent::RedrawRequested => self.present(remote),
            WindowEvent::Resized(size) => {
                if xcom_proto::debug() {
                    logln!("[ui] resized {:?} (win {remote:#x})", size);
                }
                let mut reflow = None;
                let local = (size.width as u16, size.height as u16);
                let remote_size = cap_to_display(local, self.screen);
                if remote_size != local && xcom_proto::debug() {
                    logln!(
                        "[ui] window {remote:#x} {local:?} exceeds the session display; capping to {remote_size:?}"
                    );
                }
                if let Some(att) = self.attached.get_mut(&remote) {
                    att.local = local;
                    let _ = att.target.resize(
                        NonZeroU32::new(size.width.max(1)).unwrap(),
                        NonZeroU32::new(size.height.max(1)).unwrap(),
                    );
                    if !size_close(remote_size, att.remote) {
                        // The user resized us: ask the app to reflow. Small
                        // deltas are WM/toolkit rounding, not user actions.
                        att.remote = remote_size;
                        reflow = Some(Message::Resize {
                            id: remote,
                            w: remote_size.0,
                            h: remote_size.1,
                        });
                    }
                }
                self.present(remote);
                if let Some(msg) = reflow {
                    self.send(msg);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = (position.x, position.y);
                if self.dialog.as_ref().is_some_and(|d| d.window == remote) {
                    let local = self
                        .attached
                        .get(&remote)
                        .map(|a| a.local)
                        .unwrap_or((0, 0));
                    let hover = crate::hud::close_dialog_hit(
                        local.0 as usize,
                        local.1 as usize,
                        position.x,
                        position.y,
                    );
                    let changed = self.dialog.as_mut().is_some_and(|d| {
                        if d.hover == hover {
                            false
                        } else {
                            d.hover = hover;
                            true
                        }
                    });
                    if changed {
                        self.present(remote);
                    }
                    return;
                }
                // The presented image is 1:1 at (0,0); anything beyond the
                // remote window is letterbox. Clamp, or a click in the band
                // lands outside the app on the remote root.
                let Some(pix) = self.pixels.get(&remote) else {
                    return;
                };
                let x = (position.x.round() as i32).clamp(0, pix.w.max(1) as i32 - 1) as i16;
                let y = (position.y.round() as i32).clamp(0, pix.h.max(1) as i32 - 1) as i16;
                if xcom_proto::debug() {
                    logln!("[ui] motion ({x},{y}) local={:?}", {
                        let a = self.attached.get(&remote);
                        a.map(|a| a.local)
                    });
                }
                if let Some(att) = self.attached.get_mut(&remote) {
                    att.cursor = (x, y);
                }
                self.send_input(
                    remote,
                    Message::InputMotion {
                        window: remote,
                        x,
                        y,
                    },
                );
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if self.dialog.as_ref().is_some_and(|d| d.window == remote) {
                    if state == ElementState::Pressed && button == MouseButton::Left {
                        let local = self
                            .attached
                            .get(&remote)
                            .map(|a| a.local)
                            .unwrap_or((0, 0));
                        let hit = crate::hud::close_dialog_hit(
                            local.0 as usize,
                            local.1 as usize,
                            self.pointer.0,
                            self.pointer.1,
                        );
                        match hit {
                            Some(choice) => {
                                if let Some(dialog) = self.dialog.as_mut() {
                                    dialog.focus = choice;
                                }
                                self.activate_close_dialog(el, remote);
                            }
                            None => {
                                // Click outside (or on the panel background)
                                // cancels, like Esc.
                                self.cancel_close_dialog(remote);
                            }
                        }
                    }
                    return;
                }
                let (x, y) = self
                    .attached
                    .get(&remote)
                    .map(|a| a.cursor)
                    .unwrap_or((0, 0));
                if xcom_proto::debug() {
                    logln!("[ui] button {button:?} {state:?} at ({x},{y})");
                }
                self.send_input(
                    remote,
                    Message::InputButton {
                        window: remote,
                        button: x11_button(button),
                        pressed: state == ElementState::Pressed,
                        x,
                        y,
                    },
                );
            }
            WindowEvent::MouseWheel { delta, .. } => {
                if self.dialog.as_ref().is_some_and(|d| d.window == remote) {
                    return;
                }
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
                    self.send_input(
                        remote,
                        Message::InputButton {
                            window: remote,
                            button,
                            pressed,
                            x,
                            y,
                        },
                    );
                }
            }
            WindowEvent::Focused(focused) => {
                if xcom_proto::debug() {
                    logln!("[ui] focused={focused} (win {remote:#x})");
                }
                // A remote menu dismisses itself via its X pointer grab, which
                // only sees clicks we forward. If focus leaves the session
                // (desktop, another app) the menu would stay mapped, so
                // schedule a dismiss. Any window losing focus can signal this:
                // under XWayland an override-redirect popup may never take
                // focus, so the *main* window is the one that loses it.
                if focused {
                    self.dismiss_at = None;
                } else if self.has_visible_popup() {
                    self.dismiss_at = Some(Instant::now() + POPUP_DISMISS_DELAY);
                }
                if !focused {
                    // Releases may never arrive after losing OS focus; never
                    // leave a remote key held (the X server would autorepeat it).
                    for sym in self.keys.release_all() {
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
                // The close dialog is modal: keys drive it locally and never
                // reach the remote app.
                if self.dialog.is_some() {
                    // Alt+Tab belongs to the local desktop even while the
                    // dialog is modal: switching away must not also move the
                    // dialog's focus. (The key never reaches the remote app
                    // here because the dialog branch returns below.)
                    let window_switch = crate::keys::is_window_switch(
                        &event.logical_key,
                        &event.physical_key,
                        self.mods.alt_key() || self.mods.super_key(),
                    );
                    if event.state == ElementState::Pressed && !event.repeat && !window_switch {
                        use winit::keyboard::{Key, NamedKey};
                        let window = self.dialog.as_ref().map(|d| d.window).unwrap_or(remote);
                        match event.logical_key {
                            Key::Named(NamedKey::Escape) => {
                                self.cancel_close_dialog(window);
                            }
                            Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Space) => {
                                self.activate_close_dialog(el, window);
                            }
                            Key::Named(NamedKey::Tab) | Key::Named(NamedKey::ArrowRight) => {
                                self.focus_close_dialog(window, true);
                            }
                            Key::Named(NamedKey::ArrowLeft) => {
                                self.focus_close_dialog(window, false);
                            }
                            _ => {}
                        }
                    }
                    return;
                }
                // Local test aid: dump the store exactly when the user sees the
                // artifact (not forwarded to the remote app). Opt-in.
                if self.debug_tools
                    && event.state == ElementState::Pressed
                    && !event.repeat
                    && matches!(
                        event.logical_key,
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::F12)
                    )
                {
                    logln!("[ui] F12: dumping store and requesting full refresh");
                    self.dump("mid.");
                    if let Some(&id) = self.attached.keys().next() {
                        self.send(Message::FullRequest { id });
                    }
                    return;
                }
                let action = self.keys.on_key(
                    &event.physical_key,
                    &event.logical_key,
                    event.text.as_deref(),
                    event.state == ElementState::Pressed,
                    event.repeat,
                    self.mods,
                );
                match action {
                    KeyAction::None => {}
                    KeyAction::Press(sym) => self.send_input(
                        remote,
                        Message::InputKey {
                            sym,
                            pressed: true,
                        },
                    ),
                    KeyAction::Release(sym) => self.send(Message::InputKey {
                        sym,
                        pressed: false,
                    }),
                    KeyAction::ToggleHud => {
                        let on = self.metrics.toggle_hud();
                        logln!("[ui] HUD {}", if on { "on" } else { "off" });
                        self.metrics.tick(true);
                        for att in self.attached.values() {
                            att.window.request_redraw();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        self.metrics.tick(false);
        self.dismiss_if_unfocused();
        let hud_next = if self.metrics.hud_on() {
            // Repaint so the HUD (rates, sparkline) stays live even when the
            // remote window is static and produces no frames.
            for att in self.attached.values() {
                att.window.request_redraw();
            }
            Some(Instant::now() + crate::metrics::INTERVAL)
        } else {
            None
        };
        let next = match (hud_next, self.dismiss_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        el.set_control_flow(match next {
            Some(at) => winit::event_loop::ControlFlow::WaitUntil(at),
            None => winit::event_loop::ControlFlow::Wait,
        });
    }
}

/// Grace period after a popup loses focus: focus moving between our windows
/// (e.g. the parent reacting to a click) must not dismiss the menu.
const POPUP_DISMISS_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

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

impl App {
    /// A popup that is currently shown (menus/tooltips).
    fn has_visible_popup(&self) -> bool {
        self.attached
            .iter()
            .any(|(id, a)| a.visible && self.is_popup(*id))
    }

    /// True for menu/tooltip-like windows (they behave as popups locally).
    fn is_popup(&self, id: u32) -> bool {
        self.attached.get(&id).is_some_and(|a| a.override_redirect)
            || self
                .pixels
                .get(&id)
                .is_some_and(|p| matches!(p.kind, xcom_proto::KIND_MENU | xcom_proto::KIND_TOOLTIP))
    }

    /// Escape closes a remote menu: its grab receives the injected key even
    /// though our windows no longer have local focus.
    fn dismiss_if_unfocused(&mut self) {
        let Some(at) = self.dismiss_at else {
            return;
        };
        if Instant::now() < at {
            return;
        }
        self.dismiss_at = None;
        let focused = self.attached.values().any(|a| a.window.has_focus());
        let popup = self.has_visible_popup();
        if xcom_proto::debug() {
            logln!(
                "[ui] dismiss check: focused={focused} pointer_inside={} popup={popup}",
                self.pointer_inside
            );
        }
        if focused {
            return; // focus stayed inside the session
        }
        if self.pointer_inside {
            return; // the user is interacting with one of our windows
        }
        if !popup {
            return;
        }
        logln!("[ui] focus left the session; dismissing the remote popup");
        const XK_ESCAPE: u32 = 0xff1b;
        self.send(Message::InputKey {
            sym: XK_ESCAPE,
            pressed: true,
        });
        self.send(Message::InputKey {
            sym: XK_ESCAPE,
            pressed: false,
        });
    }

    /// Debug aid: print the alpha mask and write a PPM of a received cursor.
    fn dump_cursor(&mut self, w: u16, h: u16, xhot: u16, yhot: u16, pixels: &[u32]) {
        let mut art = String::new();
        for y in 0..h {
            for x in 0..w {
                let a = (pixels[y as usize * w as usize + x as usize] >> 24) as u8;
                art.push(if a > 128 {
                    '#'
                } else if a > 0 {
                    ':'
                } else {
                    '.'
                });
            }
            art.push('\n');
        }
        logln!("[ui] cursor image {w}x{h} hot=({xhot},{yhot}):\n{art}");
        let rgba = cursor_rgba(pixels);
        let mut ppm = Vec::with_capacity(w as usize * h as usize * 3 + 32);
        ppm.extend_from_slice(format!("P6\n{w} {h}\n255\n").as_bytes());
        for px in rgba.chunks_exact(4) {
            let a = px[3] as u32;
            let blend = |c: u8| ((c as u32 * a + 255 * (255 - a)) / 255) as u8;
            ppm.extend_from_slice(&[blend(px[0]), blend(px[1]), blend(px[2])]);
        }
        self.cursor_seq += 1;
        let base = self
            .dump
            .clone()
            .unwrap_or_else(|| PathBuf::from("/tmp/xcom-cursor.ppm"));
        let path = base.with_extension(format!("cursor{}.ppm", self.cursor_seq));
        match std::fs::write(&path, &ppm) {
            Ok(()) => logln!("[ui] cursor dumped to {}", path.display()),
            Err(e) => logln!("[ui] cannot dump cursor: {e}"),
        }
    }
}

/// XFixes returns ARGB with premultiplied alpha; winit wants straight RGBA.
fn cursor_rgba(pixels: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len() * 4);
    for &px in pixels {
        let a = (px >> 24) as u8;
        if a == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let a32 = a as u32;
            let unpremultiply = |c: u32| ((c * 255 + a32 / 2) / a32).min(255) as u8;
            out.extend_from_slice(&[
                unpremultiply((px >> 16) & 0xff),
                unpremultiply((px >> 8) & 0xff),
                unpremultiply(px & 0xff),
                a,
            ]);
        }
    }
    out
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
    fn display_cap_clamps_window_growth() {
        // Interactive growth (maximize) stops at the session display.
        assert_eq!(cap_to_display((1920, 1080), Some((1600, 1200))), (1600, 1080));
        // A window that already fits is untouched.
        assert_eq!(cap_to_display((800, 600), Some((1600, 1200))), (800, 600));
        // No display information (old daemon): no cap.
        assert_eq!(cap_to_display((1920, 1080), None), (1920, 1080));
    }

    #[test]
    fn popup_origin_prefers_the_client_area() {
        let outer = Some((100, 100));
        let inner = Some((108, 131));
        // Undecorated child: anchor to the client area, so the parent's title
        // bar and border do not shift the menu.
        assert_eq!(parent_origin(outer, inner, (0, 0), true), (108, 131));
        // Decorated child: the decorations cancel, keep the outer origin.
        assert_eq!(parent_origin(outer, inner, (0, 0), false), (100, 100));
        // Platform without client-area positions (Wayland): outer, then placed.
        assert_eq!(parent_origin(outer, None, (0, 0), true), (100, 100));
        assert_eq!(parent_origin(None, None, (7, 9), true), (7, 9));
    }

    #[test]
    fn placement_cascades_and_centers() {
        // Normal window: remote position with a 32 px cascade; negatives clamp.
        assert_eq!(place_window(10, 20, xcom_proto::KIND_NORMAL, false, 100, 80, None), (42, 52));
        assert_eq!(place_window(-5, -7, xcom_proto::KIND_NORMAL, false, 100, 80, None), (32, 32));
        // Dialog asking for (0,0): centered on the parent.
        let parent = Some(((100, 100), (300u16, 200u16), (5i16, 5i16)));
        assert_eq!(
            place_window(0, 0, xcom_proto::KIND_DIALOG, false, 100, 80, parent),
            (200, 160)
        );
        // Menu: keeps the offset the remote windows have from each other.
        assert_eq!(
            place_window(15, 25, xcom_proto::KIND_MENU, false, 50, 40, parent),
            (110, 120)
        );
        // A dialog with a real position also keeps the offset (not centered).
        assert_eq!(
            place_window(15, 25, xcom_proto::KIND_DIALOG, false, 50, 40, parent),
            (110, 120)
        );
        // Override-redirect popups too, whatever their kind.
        assert_eq!(
            place_window(15, 25, xcom_proto::KIND_OTHER, true, 50, 40, parent),
            (110, 120)
        );
    }

    #[test]
    fn blit_places_rects_with_the_store_stride() {
        use xcom_proto::WireRect;
        // Two BGRA pixels, one 2x1 damage rect at (1,1) in a 4x3 frame.
        let src: [u8; 8] = [10, 20, 30, 255, 40, 50, 60, 255];
        let mut enc = xcom_codec::Compressor::new();
        let (codec, data) = enc.encode(&src);
        let frame = DamageFrame {
            window: 7,
            win_w: 4,
            win_h: 3,
            t_event_ns: 0,
            t_capture_ns: 0,
            rects: vec![WireRect {
                x: 1,
                y: 1,
                w: 2,
                h: 1,
                codec,
                raw_len: 8,
                data: data.to_vec(),
            }],
        };
        let mut p = Pixels::new(4, 3);
        let mut decode = Vec::new();
        p.blit(&frame, &mut decode);
        assert_eq!(p.buf[4 + 1], 0xff1e140a, "first source pixel at (1,1)");
        assert_eq!(p.buf[4 + 2], 0xff3c3228, "second source pixel at (2,1)");
        assert_eq!(p.buf[0], 0xff00_0000, "outside the rect stays black");
        assert_eq!(p.buf[2 * 4 + 1], 0xff00_0000, "other rows untouched");
        // A frame with a different size resizes the store first.
        let bigger = DamageFrame { win_w: 6, win_h: 5, ..frame };
        p.blit(&bigger, &mut decode);
        assert_eq!((p.w, p.h), (6, 5));
        assert_eq!(p.buf[6 + 1], 0xff1e140a, "content kept at new stride");
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
