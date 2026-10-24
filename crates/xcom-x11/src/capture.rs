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
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use x11rb::errors::ReplyError;
use x11rb::connection::Connection;
use x11rb::protocol::composite::{self, ConnectionExt as _};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, EventMask, Window};
use x11rb::protocol::{ErrorKind, Event};
use x11rb::rust_connection::RustConnection;

use xcom_codec::{coalesce, Compressor, Rect};
use xcom_proto::{now_ns, DamageFrame, WireRect};
use xcom_proto::logln;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DamageMode {
    Raw,
    NonEmpty,
}

pub struct CaptureConfig {
    pub mode: DamageMode,
    pub max_rects: usize,
    /// Point the root window at the classic left_ptr arrow. A bare X server
    /// has no desktop to do it, and widgets without their own cursor inherit
    /// the root cursor (which is otherwise the X glyph).
    pub root_arrow: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            mode: DamageMode::NonEmpty,
            max_rects: 32,
            root_arrow: true,
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
    /// A tracked window changed size or position.
    Resized {
        window: Window,
        w: u16,
        h: u16,
    },
    /// The pointer image changed (ARGB premultiplied, row-major).
    Cursor {
        xhot: u16,
        yhot: u16,
        w: u16,
        h: u16,
        pixels: Vec<u32>,
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
    /// Visible right now. Unmapped windows stay tracked (their apps may map
    /// them again) but produce no frames and are re-synced on remap.
    mapped: bool,
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
    last_cursor_serial: u32,
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

        if cfg.root_arrow {
            // Must happen before the initial cursor is read below.
            set_root_arrow(&conn, root).unwrap_or_else(|e| {
                logln!("[capture] root cursor: {e:#}");
            });
        }

        // Pointer shape changes (XFixes): the client mirrors them locally.
        conn.xfixes_select_cursor_input(root, xfixes::CursorNotifyMask::DISPLAY_CURSOR)?;

        // One full-screen 1080p window needs ~8 MB; growth is handled per rect.
        let shm = attach_shm(&conn, 8 << 20)?;
        conn.flush()?;

        let mut session = CaptureSession {
            conn,
            screen: screen_num,
            root,
            windows: HashMap::new(),
            shm,
            compressor: Compressor::new(),
            queue: VecDeque::new(),
            cfg,
            // Force the initial image through; notifications dedupe afterwards.
            last_cursor_serial: u32::MAX,
        };
        session.read_cursor()?;
        Ok(session)
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

    /// Size of the X screen: the coordinate space and the pointer bounds.
    ///
    /// Windows are capped at this: the X pointer cannot leave the screen, so a
    /// larger window has a region that can be seen but never clicked.
    pub fn screen_size(&self) -> (u16, u16) {
        let root = &self.conn.setup().roots[self.screen];
        (root.width_in_pixels, root.height_in_pixels)
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

    /// Start capturing a top-level window. Returns its current geometry, or
    /// `None` for windows that are not real content (input-only windows and
    /// tiny helper windows such as GTK's 1x1 grab window).
    pub fn add_window(&mut self, win: Window) -> Result<Option<(i16, i16, u16, u16)>> {
        if let Some(s) = self.windows.get(&win) {
            return Ok(Some((s.x, s.y, s.w, s.h)));
        }
        let geom = self
            .conn
            .get_geometry(win)
            .context("get_geometry")?
            .reply()
            .context("window does not exist")?;
        let attrs = self
            .conn
            .get_window_attributes(win)
            .context("get_window_attributes")?
            .reply()
            .context("window does not exist")?;
        if attrs.class == xproto::WindowClass::INPUT_ONLY {
            if xcom_proto::debug() {
                logln!("[capture] ignoring input-only window {win:#x}");
            }
            return Ok(None);
        }
        // Toolkits map tiny helper windows for grabs/selections; they are not
        // content and would show up as black boxes (our client enforces a
        // minimum native window size).
        const MIN_DIM: u16 = 8;
        if geom.width < MIN_DIM || geom.height < MIN_DIM {
            if xcom_proto::debug() {
                logln!(
                    "[capture] ignoring helper window {win:#x} ({}x{})",
                    geom.width, geom.height
                );
            }
            return Ok(None);
        }

        let pixmap: xproto::Pixmap = self.conn.generate_id()?;
        self.conn.composite_name_window_pixmap(win, pixmap)?;

        let damage_id: damage::Damage = self.conn.generate_id()?;
        let level = match self.cfg.mode {
            DamageMode::Raw => damage::ReportLevel::RAW_RECTANGLES,
            DamageMode::NonEmpty => damage::ReportLevel::NON_EMPTY,
        };
        self.conn.damage_create(damage_id, win, level)?;

        let parts: xfixes::Region = self.conn.generate_id()?;
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
                mapped: true,
                pending: Vec::new(),
                need_fetch: false,
                t_first_event_ns: 0,
            },
        );
        self.conn.flush()?;
        Ok(Some((geom.x, geom.y, geom.width, geom.height)))
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
        if !state.mapped {
            return Ok(None);
        }
        let full = Rect::new(0, 0, state.w, state.h);
        let t = now_ns();
        let (frame, stats) = self.capture_frame(win, vec![full], t)?;
        Ok(Some((frame, stats)))
    }

    /// Wait for the next display event (bounded by `deadline`, if any).
    ///
    /// Blocks in `poll(2)` on the X connection instead of the old 50–250 µs
    /// sleep loop: an idle session now costs no CPU. The wait is capped at
    /// 50 ms so the caller's shutdown/timeout checks still run.
    pub fn pump(&mut self, deadline: Option<Instant>) -> Result<Option<PumpEvent>> {
        if let Some(ev) = self.queue.pop_front() {
            return Ok(Some(ev));
        }

        let fd = self.conn.stream().as_raw_fd();
        let first = loop {
            // Drain anything x11rb has already buffered before sleeping.
            if let Some(ev) = self.conn.poll_for_event().context("X event stream")? {
                break ev;
            }
            let now = Instant::now();
            if let Some(d) = deadline {
                if now >= d {
                    return Ok(None);
                }
            }
            let wait_ms = deadline
                .map(|d| d.saturating_duration_since(now).as_millis().clamp(1, 50) as i32)
                .unwrap_or(50);
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, wait_ms) };
            if rc < 0 {
                let e = std::io::Error::last_os_error();
                if e.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(anyhow::anyhow!("poll on X connection: {e}"));
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
                // ConfigureNotify coordinates are parent-relative. Under a WM
                // the top-level is reparented into a frame, so translating to
                // the root is what keeps input injection and the client's
                // geometry correct (M6). No WM: the translation is a no-op.
                let (root_x, root_y) = self
                    .conn
                    .translate_coordinates(e.window, self.root, 0, 0)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|r| (r.dst_x, r.dst_y))
                    .unwrap_or((e.x, e.y));
                let (moved, resized) = {
                    let Some(state) = self.windows.get_mut(&e.window) else {
                        return Ok(());
                    };
                    let moved = root_x != state.x || root_y != state.y;
                    let resized = e.width != state.w || e.height != state.h;
                    state.x = root_x;
                    state.y = root_y;
                    (moved, resized)
                };
                if resized {
                    self.on_resize(e.window, e.width, e.height)?;
                }
                // Moves matter for dialogs/popups: toolkits map them first and
                // then move them into place.
                if moved || resized {
                    self.queue.push_back(PumpEvent::Resized {
                        window: e.window,
                        w: e.width,
                        h: e.height,
                    });
                }
            }
            Event::MapNotify(e) => {
                if e.event != self.root {
                    return Ok(());
                }
                let remapped = match self.windows.get_mut(&e.window) {
                    Some(state) => {
                        let remapped = !state.mapped;
                        state.mapped = true;
                        Some(remapped)
                    }
                    None => None,
                };
                match remapped {
                    // Already tracked: the window was hidden, announce it again.
                    Some(true) => {
                        // The named backing pixmap can be invalidated while the
                        // window is unmapped; re-name it before the client
                        // asks for a full frame.
                        if let Err(err) = self.rename_pixmap(e.window) {
                            if xcom_proto::debug() {
                                logln!("[capture] remap {:#x}: {err}", e.window);
                            }
                            self.remove_window(e.window);
                        } else {
                            self.queue.push_back(PumpEvent::Mapped(e.window));
                        }
                    }
                    Some(false) => {}
                    None => {
                        if let Ok(Some(_)) = self.add_window(e.window) {
                            self.queue.push_back(PumpEvent::Mapped(e.window));
                        }
                    }
                }
            }
            Event::UnmapNotify(e) => {
                if e.event != self.root {
                    return Ok(());
                }
                let newly_hidden = match self.windows.get_mut(&e.window) {
                    Some(state) if state.mapped => {
                        state.mapped = false;
                        true
                    }
                    _ => false,
                };
                if newly_hidden {
                    // Unmap is not destroy: keep the window and its store, and
                    // re-sync whenever the app maps it again.
                    self.queue.push_back(PumpEvent::Unmapped(e.window));
                }
            }
            Event::DestroyNotify(e) if self.windows.contains_key(&e.window) => {
                self.remove_window(e.window);
                self.queue.push_back(PumpEvent::Destroyed(e.window));
            }
            Event::XfixesCursorNotify(e)
                if e.subtype == xfixes::CursorNotify::DISPLAY_CURSOR =>
            {
                self.read_cursor()?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Fetch the current pointer image (XFixes) and queue it if it changed.
    fn read_cursor(&mut self) -> Result<()> {
        let reply = self.conn.xfixes_get_cursor_image()?.reply()?;
        if reply.width == 0 || reply.height == 0 {
            return Ok(());
        }
        if reply.width > xcom_proto::MAX_CURSOR_DIM || reply.height > xcom_proto::MAX_CURSOR_DIM {
            if xcom_proto::debug() {
                logln!(
                    "[capture] cursor {}x{} exceeds the wire limit; ignored",
                    reply.width, reply.height
                );
            }
            return Ok(());
        }
        if reply.cursor_serial == self.last_cursor_serial {
            return Ok(());
        }
        let n = reply.width as usize * reply.height as usize;
        if reply.cursor_image.len() < n {
            return Ok(());
        }
        self.last_cursor_serial = reply.cursor_serial;
        self.queue.push_back(PumpEvent::Cursor {
            xhot: reply.xhot,
            yhot: reply.yhot,
            w: reply.width,
            h: reply.height,
            pixels: reply.cursor_image[..n].to_vec(),
        });
        Ok(())
    }

    /// Queue the current pointer image even if its serial has not changed.
    ///
    /// A reattaching client has no cursor state, but the serial check above
    /// would suppress a resend until the pointer image changes. Called once per
    /// attachment, before the capture loop starts forwarding events.
    pub fn refresh_cursor(&mut self) -> Result<()> {
        self.last_cursor_serial = u32::MAX;
        self.read_cursor()
    }

    /// Turn pending damage into one captured frame and queue it.
    fn capture_if_ready(&mut self, win: Window) -> Result<()> {
        let (t_event_ns, mut rects, need_fetch, w, h, mapped) = {
            let Some(state) = self.windows.get_mut(&win) else {
                return Ok(());
            };
            let t = state.t_first_event_ns;
            state.t_first_event_ns = 0;
            let need = std::mem::take(&mut state.need_fetch);
            (
                t,
                std::mem::take(&mut state.pending),
                need,
                state.w,
                state.h,
                state.mapped,
            )
        };
        if need_fetch {
            // Clear the server-side damage even when hidden, so a remap does
            // not replay a burst of stale rectangles.
            self.fetch_damage(win, &mut rects)?;
        }
        if !mapped {
            return Ok(());
        }
        rects.retain_mut(|r| {
            *r = r.clamped(w, h);
            !r.is_empty()
        });
        coalesce(&mut rects, self.cfg.max_rects);
        // Toolkits (XRender glyph boxes in particular) can report damage a
        // pixel narrower than the pixels that actually changed, leaving thin
        // vertical slivers of erased text behind. One pixel of margin is
        // nearly free and covers the rounding.
        for r in &mut rects {
            *r = r.padded(1).clamped(w, h);
        }
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

    /// Re-create the named backing pixmap (e.g. after an unmap/remap).
    fn rename_pixmap(&mut self, win: Window) -> Result<()> {
        let Some(state) = self.windows.get_mut(&win) else {
            return Ok(());
        };
        let pixmap: xproto::Pixmap = self.conn.generate_id()?;
        self.conn.composite_name_window_pixmap(win, pixmap)?;
        let old = std::mem::replace(&mut state.pixmap, pixmap);
        let _ = self.conn.free_pixmap(old);
        self.conn.flush()?;
        Ok(())
    }

    fn on_resize(&mut self, win: Window, w: u16, h: u16) -> Result<()> {
        // The named pixmap keeps the size it had when named: re-name it.
        if let Some(state) = self.windows.get_mut(&win) {
            self.conn.free_pixmap(state.pixmap)?;
            let pixmap: xproto::Pixmap = self.conn.generate_id()?;
            self.conn.composite_name_window_pixmap(win, pixmap)?;
            state.pixmap = pixmap;
            state.w = w;
            state.h = h;
        }
        self.ensure_shm(w as usize * h as usize * 4)?;
        self.conn.flush()?;
        Ok(())
    }

    fn ensure_shm(&mut self, need: usize) -> Result<()> {
        let need = need.max(1 << 20);
        if need <= self.shm.len {
            return Ok(());
        }
        self.replace_shm(need)
    }

    /// Swap in a bigger segment and detach the old one from the *server* too.
    ///
    /// The SysV segment is IPC_RMID'd, so it only dies when both sides have
    /// detached; skipping the server-side detach pinned kernel memory inside
    /// Xvfb until the connection closed.
    fn replace_shm(&mut self, need: usize) -> Result<()> {
        let new = attach_shm(&self.conn, need)?;
        let old = std::mem::replace(&mut self.shm, new);
        if let Ok(cookie) = self.conn.shm_detach(old.seg) {
            let _ = cookie.check();
        }
        self.conn.flush()?;
        Ok(())
    }

    fn capture_frame(
        &mut self,
        win: Window,
        rects: Vec<Rect>,
        t_event_ns: u64,
    ) -> Result<(DamageFrame, FrameStats)> {
        if !self.windows.contains_key(&win) {
            bail!("window {win:#x} vanished mid-capture");
        }
        // Grow once for the largest rect (borrowing `state` blocks doing it
        // from inside the batch loop).
        let max_need = rects
            .iter()
            .map(|r| r.area() * 4)
            .max()
            .unwrap_or(0);
        if max_need > self.shm.len {
            self.replace_shm(max_need)?;
        }
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

            // ShmGetImage fails with BadMatch when the image does not fit the
            // segment, and the first rect of a batch was never checked: grow
            // instead of dropping a frame (big/just-resized windows).
            let mut cookies = Vec::with_capacity(batch.len());
            for (r, off) in &batch {
                let cookie = self.conn.shm_get_image(
                    state.pixmap,
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
                match cookie.reply() {
                    Ok(_) => {}
                    // The window was destroyed or unmapped between the damage
                    // event and the readback; its pixmap is gone. The window
                    // is disappearing anyway, so this is benign.
                    Err(ReplyError::X11Error(x)) if x.error_kind == ErrorKind::Drawable => {
                        if xcom_proto::debug() {
                            logln!(
                                "[capture] {win:#x}: drawable gone mid-capture (rect {}x{}+{}+{}, skipped)",
                                r.w, r.h, r.x, r.y
                            );
                        }
                        continue;
                    }
                    Err(e) => {
                        // Anything else is a real capture bug (e.g. BadMatch
                        // from an undersized segment).
                        logln!(
                            "capture: shm_get_image failed for {win:#x} rect {}x{}+{}+{} offset={off} segment={} bytes: {e}",
                            r.w, r.h, r.x, r.y, self.shm.len
                        );
                        continue;
                    }
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
            win_w: state.w,
            win_h: state.h,
            t_event_ns,
            t_capture_ns,
            rects: wire,
        };
        Ok((frame, stats))
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        if let Ok(cookie) = self.conn.shm_detach(self.shm.seg) {
            let _ = cookie.check();
        }
        let _ = self.conn.flush();
    }
}

/// Set the classic X left_ptr arrow as the root cursor.
///
/// The cursor resource must live on a long-lived connection: X frees a
/// client's resources when it disconnects, which would reset the root cursor.
fn set_root_arrow(conn: &RustConnection, root: Window) -> Result<()> {
    let font: xproto::Font = conn.generate_id()?;
    conn.open_font(font, b"cursor")?.check()?;
    let cid: xproto::Cursor = conn.generate_id()?;
    // Cursor font glyphs come in source/mask pairs; left_ptr is 68/69.
    conn.create_glyph_cursor(
        cid, font, font, 68, 69, 0xffff, 0xffff, 0xffff, 0, 0, 0,
    )?
    .check()?;
    conn.change_window_attributes(
        root,
        &xproto::ChangeWindowAttributesAux::new().cursor(cid),
    )?
    .check()?;
    conn.close_font(font)?.check()?;
    conn.flush()?;
    Ok(())
}

/// Create a SysV shared memory segment, attach it locally and with the server.
fn attach_shm(conn: &RustConnection, len: usize) -> Result<Shm> {
    unsafe {
        let shmid = libc::shmget(libc::IPC_PRIVATE, len, libc::IPC_CREAT | 0o600);
        if shmid < 0 {
            bail!("shmget({len}) failed");
        }
        let addr = libc::shmat(shmid, std::ptr::null(), 0);
        if std::ptr::eq(addr, libc::MAP_FAILED) {
            libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
            bail!("shmat failed");
        }
        // Mark for deletion now: the segment dies with the last detach.
        libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());

        let seg: shm::Seg = conn.generate_id()?;
        // A failed attach would silently break all capture.
        conn.shm_attach(seg, shmid as u32, false)?.check()?;
        Ok(Shm {
            seg,
            map: addr as *mut u8,
            len,
        })
    }
}
