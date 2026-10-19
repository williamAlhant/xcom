//! Window capture: Composite redirect -> Damage notifications -> MIT-SHM
//! pixel readback -> LZ4.
//!
//! Pipeline shape (this is the latency-critical path):
//!
//! 1. `CompositeRedirectSubwindows(root, MANUAL)` gives every top-level a
//!    backing pixmap ([`composite_name_window_pixmap`]) that holds its pixels
//!    even when obscured.
//! 2. A `Damage` object reports what changed. Two modes:
//!    - [`DamageMode::NonEmpty`]: one event when damage appears, then the
//!      accumulated region is fetched via XFIXES (server-side coalescing).
//!    - [`DamageMode::Raw`]: every raw rect as it happens (no XFIXES round
//!      trip, but more events).
//! 3. Damaged rects are read back through one shared-memory segment
//!    (MIT-SHM), all rects of a frame pipelined in a single batch.
//! 4. Rects are coalesced and LZ4-compressed straight out of the SHM mapping —
//!    no intermediate copy.

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use x11rb::connection::Connection;
use x11rb::protocol::composite::{self, ConnectionExt as _};
use x11rb::protocol::damage::{self, ConnectionExt as _};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt as _, Window};
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

pub struct Capturer {
    conn: RustConnection,
    /// Root window of the captured screen (needed later for XTEST injection).
    pub root: Window,
    pub win: Window,
    pub width: u16,
    pub height: u16,
    pixmap: xproto::Pixmap,
    damage: damage::Damage,
    parts: xfixes::Region,
    seg: shm::Seg,
    map: *mut u8,
    map_len: usize,
    compressor: Compressor,
    pending: Vec<Rect>,
    need_fetch: bool,
    t_first_event_ns: u64,
    cfg: CaptureConfig,
}

impl Capturer {
    /// Connect to `display` (None = $DISPLAY) and start capturing `win`,
    /// which must be a top-level (child of the root window).
    pub fn new(display: Option<&str>, win: Window, cfg: CaptureConfig) -> Result<Capturer> {
        let (conn, screen_num) =
            x11rb::connect(display).context("connecting to X display")?;
        Self::from_connection(conn, screen_num, win, cfg)
    }

    pub fn from_connection(
        conn: RustConnection,
        screen_num: usize,
        win: Window,
        cfg: CaptureConfig,
    ) -> Result<Capturer> {
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

        let geom = conn
            .get_geometry(win)
            .context("get_geometry")?
            .reply()
            .context("target window does not exist")?;

        // Redirect all top-levels; this is what makes `NameWindowPixmap` valid.
        conn.composite_redirect_subwindows(root, composite::Redirect::MANUAL)
            .context("composite_redirect_subwindows")?;

        let pixmap: xproto::Pixmap = conn.generate_id()?.into();
        conn.composite_name_window_pixmap(win, pixmap)?;

        let damage_id: damage::Damage = conn.generate_id()?.into();
        let level = match cfg.mode {
            DamageMode::Raw => damage::ReportLevel::RAW_RECTANGLES,
            DamageMode::NonEmpty => damage::ReportLevel::NON_EMPTY,
        };
        conn.damage_create(damage_id, win, level)?;

        let parts: xfixes::Region = conn.generate_id()?.into();
        conn.xfixes_create_region(parts, &[])?;

        let map_len = (geom.width as usize * geom.height as usize * 4).max(1 << 20);
        let (seg, map) = attach_shm(&conn, map_len)?;

        conn.flush()?;

        Ok(Capturer {
            conn,
            root,
            win,
            width: geom.width,
            height: geom.height,
            pixmap,
            damage: damage_id,
            parts,
            seg,
            map,
            map_len,
            compressor: Compressor::new(),
            pending: Vec::new(),
            need_fetch: false,
            t_first_event_ns: 0,
            cfg,
        })
    }

    /// Block until the window has damage, then return one captured frame.
    ///
    /// Returns `None` when only non-damage events arrived (e.g. resizes) or
    /// when `deadline` passed first, in which case the caller should simply
    /// call again (and check its own deadline).
    ///
    /// x11rb has no timed event wait, so idle waiting polls with a small
    /// adaptive sleep (50..250us). This is what keeps the loop interruptible;
    /// the added latency is bounded by the sleep quantum.
    pub fn pump(&mut self, deadline: Option<Instant>) -> Result<Option<(DamageFrame, FrameStats)>> {
        let mut sleep_us = 50u64;
        let ev = loop {
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
        if !self.handle_event(ev)? {
            return Ok(None);
        }
        // Drain everything already queued so one frame covers all of it.
        while let Some(ev) = self.conn.poll_for_event().context("X event stream")? {
            self.handle_event(ev)?;
        }

        let t_event_ns = self.t_first_event_ns;
        self.t_first_event_ns = 0;

        let mut rects = std::mem::take(&mut self.pending);
        if self.need_fetch {
            self.need_fetch = false;
            self.fetch_damage(&mut rects)?;
        }
        if rects.is_empty() {
            return Ok(None);
        }

        // Clamp to the drawable and coalesce.
        let (w, h) = (self.width, self.height);
        rects.retain_mut(|r| {
            *r = r.clamped(w, h);
            !r.is_empty()
        });
        coalesce(&mut rects, self.cfg.max_rects);
        if rects.is_empty() {
            return Ok(None);
        }

        let (frame, stats) = self.capture_frame(rects, t_event_ns)?;
        Ok(Some((frame, stats)))
    }

    fn handle_event(&mut self, ev: Event) -> Result<bool> {
        match ev {
            Event::DamageNotify(e) => {
                if self.t_first_event_ns == 0 {
                    self.t_first_event_ns = now_ns();
                }
                match self.cfg.mode {
                    DamageMode::Raw => {
                        self.pending.push(Rect::new(
                            e.area.x as u16,
                            e.area.y as u16,
                            e.area.width,
                            e.area.height,
                        ));
                        self.need_fetch = true; // raw mode: just clear the region later
                    }
                    DamageMode::NonEmpty => self.need_fetch = true,
                }
                Ok(true)
            }
            Event::ConfigureNotify(e) => {
                if e.window == self.win && (e.width != self.width || e.height != self.height) {
                    self.on_resize(e.width, e.height)?;
                }
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// NonEmpty mode: turn the accumulated damage region into rects.
    /// Raw mode: events already carried the rects; this just clears the region.
    fn fetch_damage(&mut self, rects: &mut Vec<Rect>) -> Result<()> {
        let zero = xfixes::Region::from(0u32); // None
        match self.cfg.mode {
            DamageMode::Raw => {
                self.conn.damage_subtract(self.damage, zero, zero)?;
            }
            DamageMode::NonEmpty => {
                self.conn
                    .damage_subtract(self.damage, zero, self.parts)?;
                let reply = self.conn.xfixes_fetch_region(self.parts)?.reply()?;
                for r in reply.rectangles {
                    rects.push(Rect::new(
                        r.x as u16,
                        r.y as u16,
                        r.width,
                        r.height,
                    ));
                }
            }
        }
        Ok(())
    }

    fn on_resize(&mut self, w: u16, h: u16) -> Result<()> {
        self.width = w;
        self.height = h;
        // The named pixmap keeps the size it had when named: re-name it.
        self.conn.free_pixmap(self.pixmap)?;
        let pixmap: xproto::Pixmap = self.conn.generate_id()?.into();
        self.conn.composite_name_window_pixmap(self.win, pixmap)?;
        self.pixmap = pixmap;

        let need = (w as usize * h as usize * 4).max(1 << 20);
        if need > self.map_len {
            let (seg, map) = attach_shm(&self.conn, need)?;
            unsafe { libc::shmdt(self.map as *const libc::c_void) };
            self.seg = seg;
            self.map = map;
            self.map_len = need;
        }
        self.conn.flush()?;
        Ok(())
    }

    fn capture_frame(
        &mut self,
        rects: Vec<Rect>,
        t_event_ns: u64,
    ) -> Result<(DamageFrame, FrameStats)> {
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
                if !batch.is_empty() && offset + need > self.map_len as u32 {
                    break;
                }
                batch.push((r, offset));
                offset += need;
                idx += 1;
            }

            let mut cookies = Vec::with_capacity(batch.len());
            for (r, off) in &batch {
                let cookie = self.conn.shm_get_image(
                    self.pixmap.into(),
                    r.x as i16,
                    r.y as i16,
                    r.w,
                    r.h,
                    !0u32,
                    2, // ZPixmap
                    self.seg,
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
                let raw = unsafe { std::slice::from_raw_parts(self.map.add(off as usize), len) };
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
            window: self.win,
            t_event_ns,
            t_capture_ns,
            rects: wire,
        };
        Ok((frame, stats))
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        unsafe {
            libc::shmdt(self.map as *const libc::c_void);
        }
    }
}

/// Create a SysV shared memory segment, attach it locally and with the server.
fn attach_shm(conn: &RustConnection, len: usize) -> Result<(shm::Seg, *mut u8)> {
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
        Ok((seg, addr as *mut u8))
    }
}
