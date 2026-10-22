//! Client-side metrics: rates, round-trip time, server counters and the text
//! for the HUD overlay.
//!
//! Kept out of `ui.rs` so the winit plumbing and this accounting can be read
//! (and tested) separately. Everything here is display-only: nothing feeds back
//! into the protocol.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use xcom_proto::logln;

use crate::hud::Hud;

/// How often the rate snapshot and HUD text are recomputed.
pub const INTERVAL: Duration = Duration::from_millis(500);
/// Sparkline samples (one per tick).
const GRAPH_SAMPLES: usize = 48;

/// Server-side metrics for the last interval, as reported by `Message::Stats`.
#[derive(Default, Clone, Copy)]
pub struct ServerStats {
    pub interval_ms: u32,
    pub frames: u32,
    pub rects: u32,
    pub raw_bytes: u64,
    pub comp_bytes: u64,
    pub capture_us: u32,
    pub encode_us: u32,
    pub windows: u16,
}

pub struct Metrics {
    hud_on: bool,
    hud: Hud,
    stats: ServerStats,
    /// Monotonic totals used to derive the rates.
    rx_bytes: u64,
    raw_bytes: u64,
    frames_seen: u64,
    presents: u64,
    /// Latest rate snapshot.
    rate_fps: f32,
    rate_rx: f64,
    ratio: f32,
    rtt_ms: Option<u32>,
    input_ms: Option<f32>,
    rx_history: VecDeque<u32>,
    last_tick: Instant,
    last_frames: u64,
    last_rx: u64,
    last_log: Instant,
    /// Window and time of the last input we sent, for input -> present latency.
    last_input: Option<(u32, Instant)>,
}

impl Metrics {
    pub fn new(hud_on: bool) -> Self {
        Metrics {
            hud_on,
            hud: Hud::default(),
            stats: ServerStats::default(),
            rx_bytes: 0,
            raw_bytes: 0,
            frames_seen: 0,
            presents: 0,
            rate_fps: 0.0,
            rate_rx: 0.0,
            ratio: 0.0,
            rtt_ms: None,
            input_ms: None,
            rx_history: VecDeque::new(),
            last_tick: Instant::now(),
            last_frames: 0,
            last_rx: 0,
            last_log: Instant::now(),
            last_input: None,
        }
    }

    pub fn hud_on(&self) -> bool {
        self.hud_on
    }

    pub fn hud(&self) -> &Hud {
        &self.hud
    }

    /// Flip the overlay and return the new state.
    pub fn toggle_hud(&mut self) -> bool {
        self.hud_on = !self.hud_on;
        self.hud_on
    }

    pub fn presents(&self) -> u64 {
        self.presents
    }

    pub fn on_present(&mut self) {
        self.presents += 1;
    }

    /// Remember when an input event was sent (input -> present latency).
    pub fn on_input(&mut self, window: u32) {
        self.last_input = Some((window, Instant::now()));
    }

    /// Account for one damage frame: wire bytes (compressed) and the raw pixel
    /// bytes it represents; also closes the input -> present measurement when
    /// this window was the one we typed into.
    pub fn on_frame(&mut self, window: u32, wire_bytes: u64, raw_bytes: u64) {
        self.frames_seen += 1;
        self.rx_bytes += wire_bytes;
        self.raw_bytes += raw_bytes;
        if let Some((win, at)) = self.last_input {
            if win == window {
                self.input_ms = Some(at.elapsed().as_secs_f32() * 1000.0);
                self.last_input = None;
            }
        }
    }

    pub fn on_stats(&mut self, stats: ServerStats) {
        self.stats = stats;
    }

    pub fn on_rtt(&mut self, ms: u32) {
        self.rtt_ms = Some(ms);
    }

    /// Recompute the snapshot; `force` bypasses the interval (shutdown,
    /// toggles). Logs a one-line summary every 5 s.
    pub fn tick(&mut self, force: bool) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick);
        if !force && dt < INTERVAL {
            return;
        }
        let secs = dt.as_secs_f64().max(1e-3);
        let d_frames = self.frames_seen.saturating_sub(self.last_frames);
        let d_rx = self.rx_bytes.saturating_sub(self.last_rx);
        self.rate_fps = (d_frames as f64 / secs) as f32;
        self.rate_rx = d_rx as f64 / secs;
        self.ratio = if self.rx_bytes > 0 {
            self.raw_bytes as f32 / self.rx_bytes as f32
        } else {
            0.0
        };
        self.rx_history
            .push_back(self.rate_rx.min(u32::MAX as f64) as u32);
        while self.rx_history.len() > GRAPH_SAMPLES {
            self.rx_history.pop_front();
        }
        self.last_tick = now;
        self.last_frames = self.frames_seen;
        self.last_rx = self.rx_bytes;
        self.rebuild_hud();
        if now.duration_since(self.last_log) >= Duration::from_secs(5) {
            self.last_log = now;
            logln!("[ui] metrics: {}", self.line());
        }
    }

    /// One compact, greppable metrics line (also used by the e2e checks).
    pub fn line(&self) -> String {
        let rtt = self
            .rtt_ms
            .map(|m| format!("{m}ms"))
            .unwrap_or_else(|| "?".into());
        format!(
            "fps {:.1} srv-fps {:.1} rx {}/s ratio {:.1}x rtt {} cap {}us enc {}us win {}",
            self.rate_fps,
            self.server_fps(),
            fmt_rate(self.rate_rx),
            self.server_ratio(),
            rtt,
            self.stats.capture_us,
            self.stats.encode_us,
            self.stats.windows,
        )
    }

    /// Server-reported frame rate for its last interval.
    fn server_fps(&self) -> f32 {
        if self.stats.interval_ms == 0 {
            0.0
        } else {
            self.stats.frames as f32 * 1000.0 / self.stats.interval_ms as f32
        }
    }

    /// Compression ratio from the server's byte counters (falls back to the
    /// client's own wire accounting before the first `Stats` arrives).
    fn server_ratio(&self) -> f32 {
        if self.stats.comp_bytes > 0 {
            self.stats.raw_bytes as f32 / self.stats.comp_bytes as f32
        } else {
            self.ratio
        }
    }

    fn rebuild_hud(&mut self) {
        let rtt = self
            .rtt_ms
            .map(|m| format!("{m}ms"))
            .unwrap_or_else(|| "-".into());
        let input = self
            .input_ms
            .map(|m| format!("{m:.1}ms"))
            .unwrap_or_else(|| "-".into());
        let lines = vec![
            format!(
                "xcom v{}  rtt {}  win {}",
                xcom_proto::PROTO_VERSION,
                rtt,
                self.stats.windows
            ),
            format!(
                "fps {:.1}/{:.1}  rx {}/s",
                self.rate_fps,
                self.server_fps(),
                fmt_rate(self.rate_rx)
            ),
            format!(
                "x{:.1}  cap {}us  enc {}us  rects {}",
                self.server_ratio(),
                self.stats.capture_us,
                self.stats.encode_us,
                self.stats.rects
            ),
            format!("in->px {}  pres {}", input, self.presents),
        ];
        self.hud = Hud {
            lines,
            graph: self.rx_history.iter().copied().collect(),
        };
    }
}

/// Human-readable byte rate (B/s, kB/s, MB/s).
pub fn fmt_rate(bps: f64) -> String {
    if bps >= 1e6 {
        format!("{:.1}MB", bps / 1e6)
    } else if bps >= 1e3 {
        format!("{:.1}kB", bps / 1e3)
    } else {
        format!("{bps:.0}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_rate_formatting() {
        assert_eq!(fmt_rate(0.0), "0B");
        assert_eq!(fmt_rate(999.0), "999B");
        assert_eq!(fmt_rate(1500.0), "1.5kB");
        assert_eq!(fmt_rate(2_500_000.0), "2.5MB");
    }

    #[test]
    fn frame_accounting_and_input_latency() {
        let mut m = Metrics::new(false);
        m.on_input(7);
        m.on_frame(8, 100, 1000); // a different window must not close it
        assert!(m.input_ms.is_none());
        m.on_frame(7, 200, 2000);
        assert!(m.input_ms.is_some(), "frame for the typed window closes it");
        // Presents are counted for the HUD, not derived from frames.
        m.on_present();
        assert_eq!(m.presents(), 1);
    }

    #[test]
    fn stats_and_rtt_feed_the_line() {
        let mut m = Metrics::new(false);
        m.on_stats(ServerStats {
            interval_ms: 1000,
            frames: 60,
            rects: 14,
            raw_bytes: 4_000_000,
            comp_bytes: 250_000,
            capture_us: 2800,
            encode_us: 400,
            windows: 3,
        });
        m.on_rtt(12);
        m.tick(true);
        let line = m.line();
        assert!(line.contains("srv-fps 60.0"), "{line}");
        assert!(line.contains("ratio 16.0x"), "{line}");
        assert!(line.contains("rtt 12ms"), "{line}");
        assert!(line.contains("win 3"), "{line}");
    }

    #[test]
    fn tick_swaps_the_hud_snapshot() {
        let mut m = Metrics::new(true);
        assert!(m.hud_on());
        assert!(!m.toggle_hud());
        m.on_frame(1, 10, 100);
        m.tick(true);
        assert_eq!(m.hud().lines.len(), 4);
        assert!(!m.hud().graph.is_empty());
    }
}
