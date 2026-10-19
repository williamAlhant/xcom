//! Window capture: Composite redirect -> Damage notifications -> MIT-SHM
//! pixel readback -> LZ4, for any number of top-level windows on one display.
//!
//! Pipeline shape (this is the latency-critical path):
//!
//! 1. `CompositeRedirectSubwindows(root, MANUAL)` gives every top-level a
//!    backing pixmap ([`composite_name_window_pixmap`]) that holds its pixels
//!    even when obscured.
//! 2. A `Damage` object per window reports what changed. Two modes:
//!    - [`DamageMode::NonEmpty`]: one event when damage appears, then the
//!      accumulated region is fetched via XFIXES (server-side coalescing).
//!    - [`DamageMode::Raw`]: every raw rect as it happens (no XFIXES round
//!      trip, but more events).
//! 3. Damaged rects are read back through one shared-memory segment
//!    (MIT-SHM), all rects of a frame pipelined in a single batch.
//! 4. Rects are coalesced and LZ4-compressed straight out of the SHM mapping —
//!    no intermediate copy.
//!
//! Backpressure note: when the consumer of [`PumpEvent::Frame`] blocks (slow
//! network), damage accumulates in the X server's damage regions and is
//! coalesced by [`Self::pump`] on the next capture — so memory stays bounded
//! and the client always converges on the latest state ("latest wins").

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::composite::{self, ConnectionExt as _};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask, Window};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

use xcom_codec::{coalesce, Compressor, Rect};
use xcom_proto::{now_ns, DamageFrame, WireRect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamageMode {
    Raw,
    NonEmpty,
}

pub struct CaptureConfig {
    pub mode: DamageMode,
    pub max_rects: usize,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            mode: DamageMode::NonEmpty,
            max_rects: 32,
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct FrameStats {
    /// First damage event -> last pixel reply (server + readback time).
    pub capture_ns: u64,
    /// Compression time only.
    pub encode_ns: u64,
    pub raw_bytes: usize,
    pub comp_bytes: usize,
    pub nrects: usize,
}

/// One thing that happened on the display.
#[derive(Debug)]
pub enum PumpEvent {
    /// A window's damaged pixels, captured and compressed.
    Frame {
        window: Window,
        frame: DamageFrame,
        stats: FrameStats,
    },
    /// A top-level window appeared (created + mapped).
    Mapped(Window),
    Unmapped(Window),
    Destroyed(Window),
    /// A tracked window changed size (the app reflowed or was resized).
    Resized {
        window: Window,
        w: u16,
        h: u16,
    },
}

struct WinState {
    pixmap: xproto::Pixmap,
    damage: damage::Damage,
    parts: xfixes::Region,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    pending: Vec<Rect>,
    need_fetch: bool,
    t_first_event_ns: u64,
}

struct Shm {
    seg: shm::Seg,
    map: *mut u8,
    len: usize,
}

impl Drop for Shm {
    fn drop(&mut self) {
        unsafe {
            libc::shmdt(self.map as *const libc::c_void);
        }
    }
}

pub struct CaptureSession {
    conn: Arc<RustConnection>,
    /// Screen index of the captured display (needed for window management).
    pub screen: usize,
    root: Window,
    windows: HashMap<Window, WinState>,
    shm: Shm,
    compressor: Compressor,
    queue: VecDeque<PumpEvent>,
    cfg: CaptureConfig,
}

impl CaptureSession {
    /// Connect to a display and take over compositing of its top-levels.
    pub fn connect(display: Option<&str>, cfg: CaptureConfig) -> Result<CaptureSession> {
        let (conn, screen_num) = x11rb::connect(display).context("connecting to X display")?;
        Self::from_connection(conn, screen_num, cfg)
    }

    pub fn from_connection(
        conn: RustConnection,
        screen_num: usize,
        cfg: CaptureConfig,
    ) -> Result<CaptureSession> {
        let conn = Arc::new(conn);
        let root = conn.setup().roots[screen_num].root;

        for ext in ["Composite", "DAMAGE", "XFIXES", "MIT-SHM"] {
            let present = conn
                .query_extension(ext.as_bytes())
                .context("query_extension")?
                .reply()
                .context("query_extension reply")?
                .present;
            if !present {
                bail!("X extension {ext} missing on this display (needed for capture)");
            }
        }
        composite::ConnectionExt::composite_query_version(&conn, 0, 4)?.reply()?;
        damage::ConnectionExt::damage_query_version(&conn, 1, 1)?.reply()?;
        xfixes::ConnectionExt::xfixes_query_version(&conn, 5, 0)?.reply()?;

        // Redirect all top-levels: makes NameWindowPixmap valid for each of
        // them and routes their rendering into per-window backing pixmaps.
        conn.composite_redirect_subwindows(root, composite::Redirect::MANUAL)
            .context("composite_redirect_subwindows")?;

        // Lifecycle + resize notifications for every child of the root.
        conn.change_window_attributes(
            root,
            &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?;

        let shm = attach_shm(&conn, 4 << 20)?;
        conn.flush()?;

        Ok(CaptureSession {
            conn,
            screen: screen_num,
            root,
            windows: HashMap::new(),
            shm,
            compressor: Compressor::new(),
            queue: VecDeque::new(),
            cfg,
        })
    }

    pub fn connection(&self) -> Arc<RustConnection> {
        Arc::clone(&self.conn)
    }

    pub fn root(&self) -> Window {
        self.root
    }

    pub fn tracked(&self, win: Window) -> bool {
        self.windows.contains_key(&win)
    }

    pub fn windows(&self) -> impl Iterator<Item = Window> + '_ {
        self.windows.keys().copied()
    }

    /// Geometry of a tracked window in root coordinates.
    pub fn window_geometry(&self, win: Window) -> Option<(i16, i16, u16, u16)> {
        self.windows
            .get(&win)
            .map(|s| (s.x, s.y, s.w, s.h))
    }

    /// Start capturing a top-level window. Returns its current geometry.
    pub fn add_window(&mut self, win: Window) -> Result<(i16, i16, u16, u16)> {
        if self.windows.contains_key(&win) {
            let s = &self.windows[&win];
            return Ok((s.x, s.y, s.w, s.h));
        }
        let geom = self
            .conn
            .get_geometry(win)
            .context("get_geometry")?
            .reply()
            .context("window does not exist")?;

        let pixmap: xproto::Pixmap = self.conn.generate_id()?.into();
        self.conn.composite_name_window_pixmap(win, pixmap)?;

        let damage_id: damage::Damage = self.conn.generate_id()?.into();
        let level = match self.cfg.mode {
            DamageMode::Raw => damage::ReportLevel::RAW_RECTANGLES,
            DamageMode::NonEmpty => damage::ReportLevel::NON_EMPTY,
        };
        self.conn.damage_create(damage_id, win, level)?;

        let parts: xfixes::Region = self.conn.generate_id()?.into();
        self.conn.xfixes_create_region(parts, &[])?;

        // Ensure we hear about this window's own destruction/resize even if it
        // is reparented away from the root later.
        self.conn.change_window_attributes(
            win,
            &xproto::ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?;

        self.ensure_shm(geom.width as usize * geom.height as usize * 4)?;

        self.windows.insert(
            win,
            WinState {
                pixmap,
                damage: damage_id,
                parts,
                x: geom.x,
                y: geom.y,
                w: geom.width,
                h: geom.height,
                pending: Vec::new(),
                need_fetch: false,
                t_first_event_ns: 0,
            },
        );
        self.conn.flush()?;
        Ok((geom.x, geom.y, geom.width, geom.height))
    }

    pub fn remove_window(&mut self, win: Window) {
        if let Some(state) = self.windows.remove(&win) {
            let _ = self.conn.free_pixmap(state.pixmap);
            let _ = self.conn.damage_destroy(state.damage);
            let _ = self.conn.xfixes_destroy_region(state.parts);
            let _ = self.conn.flush();
        }
    }

    /// Capture a full-window refresh frame (e.g. on attach or after a resize).
    pub fn capture_full(&mut self, win: Window) -> Result<Option<(DamageFrame, FrameStats)>> {
        let Some(state) = self.windows.get(&win) else {
            return Ok(None);
        };
        let full = Rect::new(0, 0, state.w, state.h);
        let t = now_ns();
        let (frame, stats) = self.capture_frame(win, vec![full], t)?;
        Ok(Some((frame, stats)))
    }

    /// Wait for the next display event (bounded by `deadline`, if any).
    ///
    /// x11rb has no timed event wait, so idle waiting polls with a small
    /// adaptive sleep (50..250us); this is what keeps the loop interruptible
    /// and bounds added latency by the sleep quantum.
    pub fn pump(&mut self, deadline: Option<Instant>) -> Result<Option<PumpEvent>> {
        if let Some(ev) = self.queue.pop_front() {
            return Ok(Some(ev));
        }

        let mut sleep_us = 50u64;
        let first = loop {
            if let Some(d) = deadline {
                if Instant::now() >= d {
                    return Ok(None);
                }
            }
            match self.conn.poll_for_event().context("X event stream")? {
                Some(ev) => break ev,
                None => {
                    std::thread::sleep(Duration::from_micros(sleep_us));
                    sleep_us = (sleep_us * 2).min(250);
                }
            }
        };
        self.handle_event(first)?;
        // Drain everything already queued so one frame covers all of it.
        while let Some(ev) = self.conn.poll_for_event().context("X event stream")? {
            self.handle_event(ev)?;
        }
        Ok(self.queue.pop_front())
    }

    fn handle_event(&mut self, ev: Event) -> Result<()> {
        match ev {
            Event::DamageNotify(e) => {
                let win = e.drawable;
                {
                    let Some(state) = self.windows.get_mut(&win) else {
                        return Ok(());
                    };
                    if state.t_first_event_ns == 0 {
                        state.t_first_event_ns = now_ns();
                    }
                    if self.cfg.mode == DamageMode::Raw {
                        state.pending.push(Rect::new(
                            e.area.x as u16,
                            e.area.y as u16,
                            e.area.width,
                            e.area.height,
                        ));
                    }
                    state.need_fetch = true;
                }
                self.capture_if_ready(win)?;
            }
            Event::ConfigureNotify(e) => {
                let resized = {
                    let Some(state) = self.windows.get_mut(&e.window) else {
                        return Ok(());
                    };
                    state.x = e.x;
                    state.y = e.y;
                    e.width != state.w || e.height != state.h
                };
                if resized {
                    self.on_resize(e.window, e.width, e.height)?;
                }
            }
            Event::MapNotify(e) => {
                if e.event == self.root && !self.windows.contains_key(&e.window) {
                    if let Ok((_, _, w, h)) = self.add_window(e.window) {
                        if w > 0 && h > 0 {
                            self.queue.push_back(PumpEvent::Mapped(e.window));
                        }
                    }
                }
            }
            Event::UnmapNotify(e) => {
                if e.event == self.root && self.windows.contains_key(&e.window) {
                    self.queue.push_back(PumpEvent::Unmapped(e.window));
                }
            }
            Event::DestroyNotify(e) => {
                if self.windows.contains_key(&e.window) {
                    self.remove_window(e.window);
                    self.queue.push_back(PumpEvent::Destroyed(e.window));
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Turn pending damage into one captured frame and queue it.
    fn capture_if_ready(&mut self, win: Window) -> Result<()> {
        let (t_event_ns, mut rects, need_fetch, w, h) = {
            let Some(state) = self.windows.get_mut(&win) else {
                return Ok(());
            };
            let t = state.t_first_event_ns;
            state.t_first_event_ns = 0;
            let need = std::mem::take(&mut state.need_fetch);
            (t, std::mem::take(&mut state.pending), need, state.w, state.h)
        };
        if need_fetch {
            self.fetch_damage(win, &mut rects)?;
        }
        rects.retain_mut(|r| {
            *r = r.clamped(w, h);
            !r.is_empty()
        });
        coalesce(&mut rects, self.cfg.max_rects);
        if rects.is_empty() {
            return Ok(());
        }
        let (frame, stats) = self.capture_frame(win, rects, t_event_ns)?;
        self.queue.push_back(PumpEvent::Frame {
            window: win,
            frame,
            stats,
        });
        Ok(())
    }

    /// NonEmpty mode: turn the accumulated damage region into rects.
    /// Raw mode: events already carried the rects; this just clears the region.
    fn fetch_damage(&mut self, win: Window, rects: &mut Vec<Rect>) -> Result<()> {
        let zero = xfixes::Region::from(0u32); // None
        let Some(state) = self.windows.get_mut(&win) else {
            return Ok(());
        };
        match self.cfg.mode {
            DamageMode::Raw => {
                self.conn.damage_subtract(state.damage, zero, zero)?;
            }
            DamageMode::NonEmpty => {
                self.conn.damage_subtract(state.damage, zero, state.parts)?;
                let reply = self.conn.xfixes_fetch_region(state.parts)?.reply()?;
                for r in reply.rectangles {
                    rects.push(Rect::new(r.x as u16, r.y as u16, r.width, r.height));
                }
            }
        }
        Ok(())
    }

    fn on_resize(&mut self, win: Window, w: u16, h: u16) -> Result<()> {
        // The named pixmap keeps the size it had when named: re-name it.
        if let Some(state) = self.windows.get_mut(&win) {
            self.conn.free_pixmap(state.pixmap)?;
            let pixmap: xproto::Pixmap = self.conn.generate_id()?.into();
            self.conn.composite_name_window_pixmap(win, pixmap)?;
            state.pixmap = pixmap;
            state.w = w;
            state.h = h;
        }
        self.ensure_shm(w as usize * h as usize * 4)?;
        self.conn.flush()?;
        self.queue.push_back(PumpEvent::Resized { window: win, w, h });
        Ok(())
    }

    fn ensure_shm(&mut self, need: usize) -> Result<()> {
        let need = need.max(1 << 20);
        if need <= self.shm.len {
            return Ok(());
        }
        let shm = attach_shm(&self.conn, need)?;
        self.shm = shm;
        Ok(())
    }

    fn capture_frame(
        &mut self,
        win: Window,
        rects: Vec<Rect>,
        t_event_ns: u64,
    ) -> Result<(DamageFrame, FrameStats)> {
        let Some(state) = self.windows.get_mut(&win) else {
            bail!("window {win:#x} vanished mid-capture");
        };
        let mut stats = FrameStats {
            nrects: rects.len(),
            ..Default::default()
        };
        let mut wire: Vec<WireRect> = Vec::with_capacity(rects.len());
        let mut capture_ns = 0u64;
        let mut encode_ns = 0u64;

        // Process rects in batches that fit the SHM segment; within a batch all
        // readback requests are issued first and awaited after (pipelining).
        let mut idx = 0;
        while idx < rects.len() {
            let mut batch: Vec<(Rect, u32)> = Vec::new();
            let mut offset = 0u32;
            while idx < rects.len() {
                let r = rects[idx];
                let need = (r.area() * 4) as u32;
                if !batch.is_empty() && offset + need > self.shm.len as u32 {
                    break;
                }
                batch.push((r, offset));
                offset += need;
                idx += 1;
            }

            let mut cookies = Vec::with_capacity(batch.len());
            for (r, off) in &batch {
                let cookie = self.conn.shm_get_image(
                    state.pixmap.into(),
                    r.x as i16,
                    r.y as i16,
                    r.w,
                    r.h,
                    !0u32,
                    2, // ZPixmap
                    self.shm.seg,
                    *off,
                )?;
                cookies.push((*r, *off, cookie));
            }

            for (r, off, cookie) in cookies {
                if let Err(e) = cookie.reply() {
                    // The window may have gone away mid-frame; skip the rect.
                    eprintln!("capture: shm_get_image failed: {e}");
                    continue;
                }
                capture_ns = now_ns();
                let len = r.area() * 4;
                let raw =
                    unsafe { std::slice::from_raw_parts(self.shm.map.add(off as usize), len) };
                let t0 = now_ns();
                let (codec, data) = self.compressor.encode(raw);
                encode_ns += now_ns() - t0;
                stats.raw_bytes += len;
                stats.comp_bytes += data.len();
                wire.push(WireRect {
                    x: r.x,
                    y: r.y,
                    w: r.w,
                    h: r.h,
                    codec,
                    raw_len: len as u32,
                    data: data.to_vec(),
                });
            }
        }

        let t_capture_ns = now_ns();
        stats.capture_ns = capture_ns.saturating_sub(t_event_ns);
        stats.encode_ns = encode_ns;
        let frame = DamageFrame {
            window: win,
            t_event_ns,
            t_capture_ns,
            rects: wire,
        };
        Ok((frame, stats))
    }
}

/// Create a SysV shared memory segment, attach it locally and with the server.
fn attach_shm(conn: &RustConnection, len: usize) -> Result<Shm> {
    unsafe {
        let shmid = libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600);
        if shmid < 0 {
            bail!("shmget({len}) failed");
        }
        let addr = libc::shmat(shmid, std::ptr::null(), 0);
        if addr == libc::MAP_FAILED as *mut libc::c_void {
            libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
            bail!("shmat failed");
        }
        // Mark for deletion now: the segment dies with the last detach.
        libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());

        let seg: shm::Seg = conn.generate_id()?;
        conn.shm_attach(seg, shmid as u32, false)?;
        Ok(Shm {
            seg,
            map: addr as *mut u8,
            len,
        })
    }
}
