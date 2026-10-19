//! Native windows for remote X windows (winit + softbuffer).

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::sync::mpsc::Sender;
use std::sync::Arc;

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
}

type Surf = softbuffer::Surface<Arc<Window>, Arc<Window>>;

struct RemoteWindow {
    window: Arc<Window>,
    surface: Surf,
    pixels: Vec<u32>,
    w: u16,
    h: u16,
    /// Size the server last told us (to avoid resize echo loops).
    remote_size: (u16, u16),
    cursor: (i16, i16),
}

pub struct App {
    input_tx: Sender<Message>,
    windows: HashMap<u32, RemoteWindow>,
    by_winit: HashMap<WindowId, u32>,
    decode_buf: Vec<u8>,
    frames: u64,
}

impl App {
    pub fn new(_proxy: EventLoopProxy<UserEvent>, input_tx: Sender<Message>) -> App {
        App {
            input_tx,
            windows: HashMap::new(),
            by_winit: HashMap::new(),
            decode_buf: Vec::new(),
            frames: 0,
        }
    }

    fn report(&self) {
        eprintln!(
            "[ui] {} window(s) open, {} frames blitted",
            self.windows.len(),
            self.frames
        );
    }

    fn open_window(
        &mut self,
        el: &ActiveEventLoop,
        id: u32,
        x: i16,
        y: i16,
        w: u16,
        h: u16,
        title: String,
    ) {
        let attrs = Window::default_attributes()
            .with_title(if title.is_empty() {
                format!("xcom {id:#x}")
            } else {
                format!("{title} — xcom")
            })
            // 1:1 pixel mapping for M1; DPI-aware scaling lands in M3.
            .with_inner_size(PhysicalSize::new(w.max(64), h.max(64)))
            .with_position(winit::dpi::PhysicalPosition::new(
                (x as i32).max(0) + 32,
                (y as i32).max(0) + 32,
            ));
        let Ok(window) = el.create_window(attrs) else {
            return;
        };
        let window = Arc::new(window);
        let Ok(context) = softbuffer::Context::new(Arc::clone(&window)) else {
            return;
        };
        let Ok(mut surface) = softbuffer::Surface::new(&context, Arc::clone(&window)) else {
            return;
        };
        let _ = surface.resize(
            NonZeroU32::new(w.max(1) as u32).unwrap(),
            NonZeroU32::new(h.max(1) as u32).unwrap(),
        );
        eprintln!("[ui] window {id:#x} opened {w}x{h} {title:?}");
        self.by_winit.insert(window.id(), id);
        self.windows.insert(
            id,
            RemoteWindow {
                window,
                surface,
                pixels: vec![0; w as usize * h as usize],
                w,
                h,
                remote_size: (w, h),
                cursor: (0, 0),
            },
        );
    }

    fn blit_frame(&mut self, frame: &DamageFrame) {
        self.frames += 1;
        let Some(rw) = self.windows.get_mut(&frame.window) else {
            return;
        };
        for rect in &frame.rects {
            let want = rect.w as usize * rect.h as usize * 4;
            if decompress(&mut self.decode_buf, rect.codec, &rect.data, want).is_err() {
                continue;
            }
            let stride = rw.w as usize;
            for row in 0..rect.h as usize {
                let dst_y = rect.y as usize + row;
                if dst_y >= rw.h as usize {
                    break;
                }
                let dst_off = dst_y * stride + rect.x as usize;
                let src_off = row * rect.w as usize * 4;
                for col in 0..rect.w as usize {
                    if dst_off + col >= rw.pixels.len() {
                        break;
                    }
                    let b = &self.decode_buf[src_off + col * 4..src_off + col * 4 + 4];
                    rw.pixels[dst_off + col] = u32::from_le_bytes(b.try_into().unwrap());
                }
            }
        }
        Self::present(rw);
    }

    fn present(rw: &mut RemoteWindow) {
        let Ok(mut buffer) = rw.surface.buffer_mut() else {
            return;
        };
        let n = buffer.len().min(rw.pixels.len());
        buffer[..n].copy_from_slice(&rw.pixels[..n]);
        let _ = buffer.present();
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
            } => self.open_window(el, id, x, y, w, h, title),
            UserEvent::Geometry { id, w, h } => {
                if let Some(rw) = self.windows.get_mut(&id) {
                    rw.remote_size = (w, h);
                    rw.w = w;
                    rw.h = h;
                    rw.pixels.resize(w as usize * h as usize, 0);
                    let _ = rw
                        .window
                        .request_inner_size(PhysicalSize::new(w.max(64), h.max(64)));
                    Self::present(rw);
                }
            }
            UserEvent::Close { id } => {
                if let Some(rw) = self.windows.remove(&id) {
                    self.by_winit.remove(&rw.window.id());
                    eprintln!("[ui] window {id:#x} closed");
                }
                if self.windows.is_empty() {
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
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(remote) = self.remote_id(id) else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => {
                self.send(Message::Bye);
                el.exit();
            }
            WindowEvent::RedrawRequested => {
                if let Some(rw) = self.windows.get_mut(&remote) {
                    Self::present(rw);
                }
            }
            WindowEvent::Resized(size) => {
                let mut reflow = None;
                if let Some(rw) = self.windows.get_mut(&remote) {
                    let (w, h) = (size.width as u16, size.height as u16);
                    let _ = rw.surface.resize(
                        NonZeroU32::new(size.width.max(1)).unwrap(),
                        NonZeroU32::new(size.height.max(1)).unwrap(),
                    );
                    if (w, h) != rw.remote_size {
                        // The user resized us: ask the app to reflow.
                        rw.remote_size = (w, h);
                        rw.w = w;
                        rw.h = h;
                        rw.pixels.resize(w as usize * h as usize, 0);
                        reflow = Some(Message::Resize {
                            id: remote,
                            w,
                            h,
                        });
                    }
                    Self::present(rw);
                }
                if let Some(msg) = reflow {
                    self.send(msg);
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let x = position.x.round() as i16;
                let y = position.y.round() as i16;
                if let Some(rw) = self.windows.get_mut(&remote) {
                    rw.cursor = (x, y);
                }
                self.send(Message::InputMotion {
                    window: remote,
                    x,
                    y,
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let (x, y) = self
                    .windows
                    .get(&remote)
                    .map(|rw| rw.cursor)
                    .unwrap_or((0, 0));
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
                    .windows
                    .get(&remote)
                    .map(|rw| rw.cursor)
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
            WindowEvent::KeyboardInput { event, .. } => {
                if event.repeat {
                    return;
                }
                if let Some(sym) = keysym_for(&event.logical_key) {
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
