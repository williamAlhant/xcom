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

use crate::keys::keysym_for;

/// Events from the network threads to the UI thread.
pub enum UserEvent {
    Open {
        id: u32,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        title: String,
    },
    Geometry {
        id: u32,
        w: u16,
        h: u16,
    },
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
        }
    }

    fn resize(&mut self, w: u16, h: u16) {
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
    dump: Option<PathBuf>,
    fullscreen: bool,
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
    ) -> App {
        App {
            input_tx,
            pixels: HashMap::new(),
            attached: HashMap::new(),
            by_winit: HashMap::new(),
            decode_buf: Vec::new(),
            frames_blitted: 0,
            presents: 0,
            present_errors: 0,
            dump,
            fullscreen,
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
            path = base.with_extension(format!("{suffix}ppm"));
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

    fn open_window(&mut self, el: &ActiveEventLoop, id: u32) {
        if self.attached.contains_key(&id) {
            return;
        }
        // The pixel store must exist by now (create message or first frame);
        // create it from the create message if the frame hasn't arrived yet.
        let Some(pix) = self.pixels.get(&id) else {
            return;
        };
        let (w, h) = (pix.w.max(64), pix.h.max(64));
        let title = if pix.title.is_empty() {
            format!("xcom {id:#x}")
        } else {
            format!("{} — xcom", pix.title)
        };
        let attrs = Window::default_attributes()
            .with_title(title)
            .with_fullscreen(if self.fullscreen {
                Some(winit::window::Fullscreen::Borderless(None))
            } else {
                None
            })
            // 1:1 pixel mapping for now; DPI-aware scaling lands with M3.
            .with_inner_size(PhysicalSize::new(w, h))
            .with_position(winit::dpi::PhysicalPosition::new(
                (pix.x as i32).max(0) + 32,
                (pix.y as i32).max(0) + 32,
            ));
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
            "[ui] window {id:#x} opened {w}x{h} (backend: {:?})",
            window
                .window_handle()
                .map(|h| format!("{:?}", h.as_raw()))
                .unwrap_or_else(|e| format!("unknown: {e}"))
        );
        self.by_winit.insert(window.id(), id);
        self.attached.insert(
            id,
            Attached {
                window,
                target,
                local: (w, h),
                remote: (pix.w, pix.h),
                cursor: (0, 0),
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
            } => {
                let pix = self.pixels.entry(id).or_insert_with(|| Pixels::new(w, h));
                pix.x = x;
                pix.y = y;
                pix.title = title;
                // If the size came from a frame first, trust the create message.
                pix.resize(w, h);
                self.open_window(el, id);
            }
            UserEvent::Geometry { id, w, h } => {
                if xcom_proto::debug() {
                    eprintln!("[ui] geometry {id:#x} -> {w}x{h}");
                }
                if let Some(pix) = self.pixels.get_mut(&id) {
                    pix.resize(w, h);
                }
                if let Some(att) = self.attached.get_mut(&id) {
                    att.remote = (w, h);
                    // Follow the app's size, but only when it really differs:
                    // asking the WM to round-trip a 1px change loops forever.
                    if !size_close(att.local, att.remote) {
                        let _ = att
                            .window
                            .request_inner_size(PhysicalSize::new(w.max(64), h.max(64)));
                    }
                }
                self.present(id);
            }
            UserEvent::Close { id } => {
                self.pixels.remove(&id);
                if let Some(att) = self.attached.remove(&id) {
                    self.by_winit.remove(&att.window.id());
                    eprintln!("[ui] window {id:#x} closed");
                }
                if self.attached.is_empty() {
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
            UserEvent::Dump => self.dump("mid."),
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

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(remote) = self.remote_id(id) else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                self.report();
                self.send(Message::Bye);
                el.exit();
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
                let x = position.x.round() as i16;
                let y = position.y.round() as i16;
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
                    return;
                }
                // Keyboard focus follows OS window focus: without a WM on the
                // remote display, XTEST keystrokes would go to whatever the
                // remote pointer happens to be over.
                self.send(Message::Focus { window: remote });
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.repeat {
                    return;
                }
                // Local hotkey: dump the store exactly when the user sees the
                // artifact (not forwarded to the remote app).
                if matches!(event.logical_key, winit::keyboard::Key::Named(
                    winit::keyboard::NamedKey::F12
                )) && event.state == ElementState::Pressed
                {
                    eprintln!("[ui] F12: dumping store and requesting full refresh");
                    self.dump("mid.");
                    if let Some(&id) = self.attached.keys().next() {
                        self.send(Message::FullRequest { id });
                    }
                    return;
                }
                let sym = keysym_for(&event.logical_key);
                if xcom_proto::debug() {
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
                    self.send(Message::InputKey {
                        sym,
                        pressed: event.state == ElementState::Pressed,
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
    fn rounding_is_not_a_user_resize() {
        // WM/toolkit rounding and character-grid snapping must not be
        // forwarded as resizes (that started the ping-pong).
        assert!(size_close((484, 316), (484, 316)));
        assert!(size_close((492, 320), (493, 322)));
        assert!(!size_close((484, 316), (520, 340)));
    }
}
