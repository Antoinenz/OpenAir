//! Dashboard state: everything derived from the stream's snapshot, with no
//! terminal involved.
//!
//! The stream keeps no history — it only publishes current values. This is
//! where sampling turns them into the series the graph draws, which is what
//! keeps the audio path free of any notion of a display rate.

use std::collections::VecDeque;
use std::time::Instant;

use crossterm::event::KeyCode;
use openair_client::{ReceiverStat, StreamStats};
use openair_core::metadata::NowPlaying;

use crate::settings::GraphKind;

/// Samples kept for the graph. At the 10 Hz render rate that is ~12 seconds —
/// long enough to see a dip develop, short enough to stay legible in one row.
pub const HISTORY: usize = 120;

#[derive(Debug, Clone, PartialEq)]
pub enum DashAction {
    None,
    Quit,
}

pub struct DashboardState {
    /// Milliseconds of headroom, oldest first.
    buffer_history: VecDeque<i64>,
    /// Bytes per second, oldest first.
    bandwidth_history: VecDeque<f64>,
    /// `(bytes_sent, when)` from the previous sample, for the rate difference.
    last_bytes: Option<(u64, Instant)>,
    bandwidth_bps: Option<f64>,
    pub graph: GraphKind,
    pub latency_ms: u64,
    pub receivers: Vec<ReceiverStat>,
    pub now_playing: Option<NowPlaying>,
    pub log_scroll: usize,
    /// Lowest headroom seen for the whole run — the number worth remembering
    /// after a dropout, since the graph will have scrolled past it.
    pub worst_lead_ms: Option<i64>,
}

impl DashboardState {
    pub fn new(graph: GraphKind, latency_ms: u64) -> Self {
        Self {
            buffer_history: VecDeque::with_capacity(HISTORY),
            bandwidth_history: VecDeque::with_capacity(HISTORY),
            last_bytes: None,
            bandwidth_bps: None,
            graph,
            latency_ms,
            receivers: Vec::new(),
            now_playing: None,
            log_scroll: 0,
            worst_lead_ms: None,
        }
    }

    /// Take one reading. Called once per render tick.
    pub fn sample(&mut self, stats: &StreamStats, now: Instant) {
        if let Some(lead) = stats.take_min_lead_ms() {
            push_bounded(&mut self.buffer_history, lead);
            self.worst_lead_ms = Some(match self.worst_lead_ms {
                Some(worst) => worst.min(lead),
                None => lead,
            });
        }

        let bytes = stats.bytes_sent();
        match self.last_bytes {
            // A counter that went backwards means a fresh stream, not a
            // 4-exabyte transfer: reset rather than underflow.
            Some((prev, _)) if bytes < prev => {
                self.bandwidth_bps = None;
                self.bandwidth_history.clear();
            }
            Some((prev, at)) => {
                let dt = now.saturating_duration_since(at).as_secs_f64();
                if dt > 0.0 {
                    let bps = (bytes - prev) as f64 / dt;
                    self.bandwidth_bps = Some(bps);
                    push_bounded(&mut self.bandwidth_history, bps);
                }
            }
            // First sample establishes the baseline; a rate needs two.
            None => {}
        }
        self.last_bytes = Some((bytes, now));

        self.latency_ms = stats.latency_ms();
        self.receivers = stats.receivers();
        if let Some(np) = stats.now_playing() {
            self.now_playing = Some(np);
        }
    }

    pub fn on_key(&mut self, key: KeyCode) -> DashAction {
        match key {
            KeyCode::Char('b') => {
                self.graph = self.graph.toggled();
                DashAction::None
            }
            KeyCode::Up => {
                self.log_scroll = self.log_scroll.saturating_add(1);
                DashAction::None
            }
            KeyCode::Down => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
                DashAction::None
            }
            KeyCode::Char('q') | KeyCode::Esc => DashAction::Quit,
            _ => DashAction::None,
        }
    }

    pub fn bandwidth_bps(&self) -> Option<f64> {
        self.bandwidth_bps
    }

    pub fn buffer_history(&self) -> &VecDeque<i64> {
        &self.buffer_history
    }

    pub fn bandwidth_history(&self) -> &VecDeque<f64> {
        &self.bandwidth_history
    }

    /// The series the graph is currently showing, as `u64` for ratatui's
    /// sparkline. Negative headroom clamps to zero — the bar bottoming out is
    /// the signal, and the exact depth below zero is in the logs.
    pub fn graph_series(&self) -> Vec<u64> {
        match self.graph {
            GraphKind::Buffer => self
                .buffer_history
                .iter()
                .map(|&v| v.max(0) as u64)
                .collect(),
            GraphKind::Bandwidth => self
                .bandwidth_history
                .iter()
                .map(|&v| v.max(0.0) as u64)
                .collect(),
        }
    }

    pub fn latest_lead_ms(&self) -> Option<i64> {
        self.buffer_history.back().copied()
    }

    /// Total bytes sent as of the last sample.
    pub fn bytes_total(&self) -> u64 {
        self.last_bytes.map(|(bytes, _)| bytes).unwrap_or(0)
    }
}

fn push_bounded<T>(q: &mut VecDeque<T>, v: T) {
    if q.len() == HISTORY {
        q.pop_front();
    }
    q.push_back(v);
}

/// Human-readable bytes-per-second, as a bit rate (which is how audio is
/// normally quoted).
pub fn format_bitrate(bps: Option<f64>) -> String {
    match bps {
        None => "—".to_string(),
        Some(bps) => {
            let kbit = bps * 8.0 / 1000.0;
            if kbit >= 1000.0 {
                format!("{:.2} Mbit/s", kbit / 1000.0)
            } else {
                format!("{kbit:.0} kbit/s")
            }
        }
    }
}

/// Human-readable total transferred.
pub fn format_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// `M:SS` or `H:MM:SS`.
pub fn format_elapsed(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn dash() -> DashboardState {
        DashboardState::new(GraphKind::Buffer, 500)
    }

    #[test]
    fn first_sample_yields_no_rate() {
        // A rate needs two readings; reporting one would mean dividing by the
        // whole run's elapsed time and showing a wrong, low number.
        let mut d = dash();
        let stats = StreamStats::new(500);
        stats.add_bytes(1000);
        d.sample(&stats, Instant::now());
        assert_eq!(d.bandwidth_bps(), None);
    }

    #[test]
    fn bandwidth_is_the_difference_over_the_interval() {
        let mut d = dash();
        let stats = StreamStats::new(500);
        let t0 = Instant::now();

        stats.add_bytes(1000);
        d.sample(&stats, t0);
        stats.add_bytes(4000);
        d.sample(&stats, t0 + Duration::from_secs(2));

        // 4000 bytes over 2 s = 2000 B/s.
        let bps = d.bandwidth_bps().expect("rate after two samples");
        assert!((bps - 2000.0).abs() < 1.0, "got {bps}");
    }

    #[test]
    fn a_counter_that_goes_backwards_resets_instead_of_underflowing() {
        // Two u64s subtracted the wrong way round would report an exabyte.
        let mut d = dash();
        let stats_a = StreamStats::new(500);
        stats_a.add_bytes(100_000);
        let t0 = Instant::now();
        d.sample(&stats_a, t0);
        d.sample(&stats_a, t0 + Duration::from_secs(1));
        assert!(d.bandwidth_bps().is_some());

        let stats_b = StreamStats::new(500);
        stats_b.add_bytes(10);
        d.sample(&stats_b, t0 + Duration::from_secs(2));
        assert_eq!(d.bandwidth_bps(), None, "reset, not a nonsense rate");
        assert!(d.bandwidth_history().is_empty());
    }

    #[test]
    fn zero_interval_samples_are_ignored() {
        let mut d = dash();
        let stats = StreamStats::new(500);
        let t0 = Instant::now();
        d.sample(&stats, t0);
        stats.add_bytes(500);
        d.sample(&stats, t0);
        assert_eq!(d.bandwidth_bps(), None, "no division by zero");
    }

    #[test]
    fn buffer_history_is_bounded() {
        let mut d = dash();
        let stats = StreamStats::new(500);
        let mut t = Instant::now();
        for i in 0..(HISTORY + 50) {
            stats.record_lead_ms(i as i64);
            t += Duration::from_millis(100);
            d.sample(&stats, t);
        }
        assert_eq!(d.buffer_history().len(), HISTORY);
        assert_eq!(
            *d.buffer_history().front().unwrap(),
            50,
            "oldest samples dropped"
        );
    }

    #[test]
    fn a_window_with_no_packets_adds_no_sample() {
        // Paused streams must not draw a flat line at zero.
        let mut d = dash();
        let stats = StreamStats::new(500);
        d.sample(&stats, Instant::now());
        assert!(d.buffer_history().is_empty());
    }

    #[test]
    fn worst_lead_survives_the_graph_scrolling_past_it() {
        let mut d = dash();
        let stats = StreamStats::new(500);
        let mut t = Instant::now();
        stats.record_lead_ms(20);
        d.sample(&stats, t);
        for _ in 0..HISTORY + 10 {
            stats.record_lead_ms(400);
            t += Duration::from_millis(100);
            d.sample(&stats, t);
        }
        assert_eq!(d.worst_lead_ms, Some(20));
    }

    #[test]
    fn negative_headroom_clamps_to_zero_in_the_series() {
        let mut d = dash();
        let stats = StreamStats::new(500);
        stats.record_lead_ms(-40);
        d.sample(&stats, Instant::now());
        assert_eq!(d.graph_series(), vec![0]);
        assert_eq!(d.latest_lead_ms(), Some(-40), "the real value is still there");
    }

    #[test]
    fn b_toggles_the_graph_and_the_series_follows() {
        let mut d = dash();
        let stats = StreamStats::new(500);
        let t0 = Instant::now();
        stats.record_lead_ms(300);
        d.sample(&stats, t0);
        stats.add_bytes(8000);
        stats.record_lead_ms(300);
        d.sample(&stats, t0 + Duration::from_secs(1));

        assert_eq!(d.graph, GraphKind::Buffer);
        assert_eq!(d.graph_series(), vec![300, 300]);
        d.on_key(KeyCode::Char('b'));
        assert_eq!(d.graph, GraphKind::Bandwidth);
        assert_eq!(d.graph_series(), vec![8000]);
    }

    #[test]
    fn quit_keys_report_quit() {
        let mut d = dash();
        assert_eq!(d.on_key(KeyCode::Char('q')), DashAction::Quit);
        assert_eq!(d.on_key(KeyCode::Esc), DashAction::Quit);
    }

    #[test]
    fn log_scroll_never_goes_negative() {
        let mut d = dash();
        d.on_key(KeyCode::Down);
        assert_eq!(d.log_scroll, 0);
        d.on_key(KeyCode::Up);
        assert_eq!(d.log_scroll, 1);
    }

    #[test]
    fn latency_and_receivers_come_from_the_snapshot() {
        let mut d = dash();
        let stats = StreamStats::new(500);
        stats.set_latency_ms(750);
        stats.set_receivers(vec![ReceiverStat {
            name: "Pool Room".into(),
            addr: "192.168.1.51:7000".parse().unwrap(),
            state: openair_client::ReceiverState::Connected,
            offset_ms: 0,
        }]);
        d.sample(&stats, Instant::now());
        assert_eq!(d.latency_ms, 750);
        assert_eq!(d.receivers.len(), 1);
    }

    #[test]
    fn now_playing_persists_when_the_snapshot_stops_reporting() {
        // Metadata is set once per track change, so a later sample that sees
        // nothing must not blank the panel.
        let mut d = dash();
        let stats = StreamStats::new(500);
        stats.set_now_playing(NowPlaying {
            title: "Home".into(),
            artist: "Bon Iver".into(),
            album: "22, A Million".into(),
            art: None,
        });
        d.sample(&stats, Instant::now());
        assert_eq!(d.now_playing.as_ref().unwrap().title, "Home");

        let quiet = StreamStats::new(500);
        d.sample(&quiet, Instant::now());
        assert_eq!(d.now_playing.as_ref().unwrap().title, "Home");
    }

    #[test]
    fn bitrate_formatting_switches_units() {
        assert_eq!(format_bitrate(None), "—");
        assert_eq!(format_bitrate(Some(16_000.0)), "128 kbit/s");
        assert_eq!(format_bitrate(Some(250_000.0)), "2.00 Mbit/s");
    }

    #[test]
    fn byte_and_elapsed_formatting() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1:05");
        assert_eq!(format_elapsed(Duration::from_secs(3725)), "1:02:05");
    }
}
