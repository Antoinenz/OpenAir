//! Dashboard state: everything derived from the stream's snapshot, with no
//! terminal involved.
//!
//! The stream keeps no history — it only publishes current values. This is
//! where sampling turns them into the series the graph draws, which is what
//! keeps the audio path free of any notion of a display rate.

use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use std::net::SocketAddr;

use crossterm::event::KeyCode;
use openair_client::{
    ReceiverStat, ReceiverState, StreamCommand, StreamStats, TRIM_MAX_DB, TRIM_MIN_DB,
};
use openair_core::metadata::NowPlaying;

use crate::settings::GraphKind;

/// Samples kept for the graph. At the 10 Hz render rate that is ~12 seconds —
/// long enough to see a dip develop, short enough to stay legible in one row.
pub const HISTORY: usize = 120;

#[derive(Debug, Clone, PartialEq)]
pub enum DashAction {
    None,
    Quit,
    /// Send this to the stream.
    Command(StreamCommand),
    /// Open the add-a-receiver overlay.
    OpenPicker,
}

/// Fallback device id, matching the CLI, for a receiver that advertised none.
const DEFAULT_DEVICE_ID: &str = "AA:BB:CC:DD:EE:FF";

/// Volume trim step per key press, in dB. One dB is the smallest step that is
/// reliably audible, so a key press always does something.
pub const TRIM_STEP_DB: f32 = 1.0;

/// Offset step per key press, in ms. Ten is about the smallest shift that
/// changes where a room sits in a stereo image.
pub const OFFSET_STEP_MS: i64 = 10;

/// Offset bounds. Beyond this the receiver is no longer in the same room as
/// the rest of the group in any useful sense.
pub const OFFSET_MIN_MS: i64 = -500;
pub const OFFSET_MAX_MS: i64 = 500;

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
    /// The highlighted receiver, tracked by address rather than row index.
    /// The group's membership changes underneath the cursor as receivers drop,
    /// reconnect and get added — an index would quietly come to mean a
    /// different receiver, and the next key press would hit the wrong one.
    selected: Option<SocketAddr>,
    /// Values the user has asked for but the stream hasn't confirmed yet.
    ///
    /// Without this, a held key loses steps: the UI samples at 10 Hz but key
    /// repeat is ~30 Hz, so a sample carrying the pre-keypress value would
    /// overwrite the local one and the next press would recompute from stale
    /// state. An entry is dropped as soon as the stream reports the value back.
    pending: HashMap<SocketAddr, Pending>,
    /// Device ids for receivers in this run, so a retry can rebuild the
    /// session. `ReceiverStat` carries the address but not the id, and the
    /// id is what pairing keys off.
    device_ids: HashMap<SocketAddr, String>,
}

#[derive(Debug, Default, Clone, Copy)]
struct Pending {
    trim_db: Option<f32>,
    offset_ms: Option<i64>,
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
            selected: None,
            pending: HashMap::new(),
            device_ids: HashMap::new(),
        }
    }

    /// Tell the dashboard which device id belongs to each address.
    pub fn set_device_ids(&mut self, ids: HashMap<SocketAddr, String>) {
        self.device_ids = ids;
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
        self.reconcile_pending();
        self.ensure_selection();
        if let Some(np) = stats.now_playing() {
            self.now_playing = Some(np);
        }
    }

    /// Keep locally-set values visible until the stream confirms them, then
    /// let the stream's value stand — it is the authority, and a trim it
    /// clamped or refused must show up rather than being hidden forever.
    fn reconcile_pending(&mut self) {
        self.pending.retain(|addr, pending| {
            let Some(r) = self.receivers.iter_mut().find(|r| r.addr == *addr) else {
                // Receiver is gone; so is anything pending for it.
                return false;
            };
            if let Some(want) = pending.trim_db {
                if (r.trim_db - want).abs() < f32::EPSILON {
                    pending.trim_db = None;
                } else {
                    r.trim_db = want;
                }
            }
            if let Some(want) = pending.offset_ms {
                if r.offset_ms == want {
                    pending.offset_ms = None;
                } else {
                    r.offset_ms = want;
                }
            }
            pending.trim_db.is_some() || pending.offset_ms.is_some()
        });
    }

    /// Select something sensible: the first receiver on the first sample, and
    /// a surviving neighbour if the selected one disappears.
    fn ensure_selection(&mut self) {
        if self.receivers.is_empty() {
            self.selected = None;
            return;
        }
        let still_there = self
            .selected
            .is_some_and(|addr| self.receivers.iter().any(|r| r.addr == addr));
        if !still_there {
            self.selected = Some(self.receivers[0].addr);
        }
    }

    /// Index of the highlighted receiver, or `None` when the list is empty.
    pub fn cursor(&self) -> Option<usize> {
        let selected = self.selected?;
        self.receivers.iter().position(|r| r.addr == selected)
    }

    pub fn selected_receiver(&self) -> Option<&ReceiverStat> {
        self.cursor().and_then(|i| self.receivers.get(i))
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.receivers.is_empty() {
            self.selected = None;
            return;
        }
        let current = self.cursor().unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, self.receivers.len() as isize - 1);
        self.selected = Some(self.receivers[next as usize].addr);
    }

    pub fn on_key(&mut self, key: KeyCode) -> DashAction {
        match key {
            KeyCode::Char('b') => {
                self.graph = self.graph.toggled();
                DashAction::None
            }
            KeyCode::Up => {
                self.move_cursor(-1);
                DashAction::None
            }
            KeyCode::Down => {
                self.move_cursor(1);
                DashAction::None
            }
            // Log scrolling moved off the arrow keys, which now drive the
            // receiver cursor.
            KeyCode::PageUp => {
                self.log_scroll = self.log_scroll.saturating_add(1);
                DashAction::None
            }
            KeyCode::PageDown => {
                self.log_scroll = self.log_scroll.saturating_sub(1);
                DashAction::None
            }
            KeyCode::Char('+') | KeyCode::Char('=') => self.nudge_trim(TRIM_STEP_DB),
            KeyCode::Char('-') | KeyCode::Char('_') => self.nudge_trim(-TRIM_STEP_DB),
            KeyCode::Char('>') | KeyCode::Char('.') => self.nudge_offset(OFFSET_STEP_MS),
            KeyCode::Char('<') | KeyCode::Char(',') => self.nudge_offset(-OFFSET_STEP_MS),
            KeyCode::Char('a') => DashAction::OpenPicker,
            KeyCode::Char('r') => self.retry_selected(),
            KeyCode::Char('d') | KeyCode::Delete => match self.selected_receiver() {
                Some(r) => DashAction::Command(StreamCommand::Remove { addr: r.addr }),
                None => DashAction::None,
            },
            KeyCode::Char('q') | KeyCode::Esc => DashAction::Quit,
            _ => DashAction::None,
        }
    }

    /// Try a failed receiver again.
    ///
    /// Deliberately manual: a receiver that is asleep or on another network
    /// fails identically ten seconds later, and silently retrying in the
    /// background is the unsolicited-connection behaviour the picker exists
    /// to avoid. Retrying a live receiver would tear down a working session
    /// to rebuild it, so that is a no-op rather than a surprise.
    fn retry_selected(&mut self) -> DashAction {
        let Some(r) = self.selected_receiver() else {
            return DashAction::None;
        };
        if r.state != ReceiverState::Failed && r.state != ReceiverState::Dead {
            return DashAction::None;
        }
        DashAction::Command(StreamCommand::Add {
            addr: r.addr,
            device_id: self
                .device_ids
                .get(&r.addr)
                .cloned()
                .unwrap_or_else(|| DEFAULT_DEVICE_ID.to_string()),
        })
    }

    fn nudge_trim(&mut self, delta: f32) -> DashAction {
        let Some(r) = self.selected_receiver() else {
            return DashAction::None;
        };
        let db = (r.trim_db + delta).clamp(TRIM_MIN_DB, TRIM_MAX_DB);
        let addr = r.addr;
        // Reflect it locally straight away. The stream is the authority and
        // will confirm on the next sample, but waiting ~100 ms for that makes
        // a held key feel broken.
        if let Some(i) = self.cursor() {
            self.receivers[i].trim_db = db;
        }
        self.pending.entry(addr).or_default().trim_db = Some(db);
        DashAction::Command(StreamCommand::SetTrim { addr, db })
    }

    fn nudge_offset(&mut self, delta: i64) -> DashAction {
        let Some(r) = self.selected_receiver() else {
            return DashAction::None;
        };
        let ms = (r.offset_ms + delta).clamp(OFFSET_MIN_MS, OFFSET_MAX_MS);
        let addr = r.addr;
        if let Some(i) = self.cursor() {
            self.receivers[i].offset_ms = ms;
        }
        self.pending.entry(addr).or_default().offset_ms = Some(ms);
        DashAction::Command(StreamCommand::SetOffset { addr, ms })
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

    // --- per-receiver controls ---

    fn addr(n: u8) -> SocketAddr {
        format!("192.168.1.{n}:7000").parse().unwrap()
    }

    fn receiver(n: u8, name: &str) -> ReceiverStat {
        ReceiverStat {
            name: name.into(),
            addr: addr(n),
            state: openair_client::ReceiverState::Connected,
            offset_ms: 0,
            trim_db: 0.0,
            lead_ms: None,
            health: 0.0,
            error: None,
        }
    }

    /// A dashboard with `n` receivers already sampled in.
    fn dash_with(receivers: Vec<ReceiverStat>) -> (DashboardState, std::sync::Arc<StreamStats>) {
        let mut d = dash();
        let stats = StreamStats::new(500);
        stats.set_receivers(receivers);
        d.sample(&stats, Instant::now());
        (d, stats)
    }

    #[test]
    fn the_first_receiver_is_selected_automatically() {
        let (d, _) = dash_with(vec![receiver(51, "Pool"), receiver(52, "Living")]);
        assert_eq!(d.cursor(), Some(0));
        assert_eq!(d.selected_receiver().unwrap().name, "Pool");
    }

    #[test]
    fn arrows_move_the_cursor_and_clamp() {
        let (mut d, _) = dash_with(vec![receiver(51, "Pool"), receiver(52, "Living")]);
        d.on_key(KeyCode::Up);
        assert_eq!(d.cursor(), Some(0), "already at the top");
        d.on_key(KeyCode::Down);
        assert_eq!(d.cursor(), Some(1));
        d.on_key(KeyCode::Down);
        assert_eq!(d.cursor(), Some(1), "already at the bottom");
    }

    #[test]
    fn the_cursor_follows_the_receiver_when_the_list_reorders() {
        // Same lesson as the picker: an index-keyed cursor would silently come
        // to mean a different receiver.
        let (mut d, stats) = dash_with(vec![receiver(51, "Pool"), receiver(52, "Living")]);
        d.on_key(KeyCode::Down);
        assert_eq!(d.selected_receiver().unwrap().name, "Living");

        // A new receiver arrives at the head of the list.
        stats.set_receivers(vec![
            receiver(50, "Kitchen"),
            receiver(51, "Pool"),
            receiver(52, "Living"),
        ]);
        d.sample(&stats, Instant::now());
        assert_eq!(
            d.selected_receiver().unwrap().name,
            "Living",
            "the highlight stayed with the receiver, not the row"
        );
    }

    #[test]
    fn selection_falls_back_when_the_selected_receiver_disappears() {
        let (mut d, stats) = dash_with(vec![receiver(51, "Pool"), receiver(52, "Living")]);
        d.on_key(KeyCode::Down);
        stats.set_receivers(vec![receiver(51, "Pool")]);
        d.sample(&stats, Instant::now());
        assert_eq!(d.selected_receiver().unwrap().name, "Pool");
    }

    #[test]
    fn plus_and_minus_emit_a_trim_command_for_the_selected_receiver() {
        let (mut d, _) = dash_with(vec![receiver(51, "Pool"), receiver(52, "Living")]);
        d.on_key(KeyCode::Down); // select Living

        assert_eq!(
            d.on_key(KeyCode::Char('-')),
            DashAction::Command(StreamCommand::SetTrim {
                addr: addr(52),
                db: -TRIM_STEP_DB
            })
        );
    }

    #[test]
    fn trim_clamps_at_the_bounds() {
        let mut receivers = vec![receiver(51, "Pool")];
        receivers[0].trim_db = TRIM_MAX_DB;
        let (mut d, _) = dash_with(receivers);
        assert_eq!(
            d.on_key(KeyCode::Char('+')),
            DashAction::Command(StreamCommand::SetTrim {
                addr: addr(51),
                db: TRIM_MAX_DB
            })
        );
    }

    #[test]
    fn a_held_key_keeps_stepping_despite_a_stale_sample() {
        // The real failure this guards: key repeat is faster than the 10 Hz
        // sample rate, so a sample carrying the pre-keypress value must not
        // overwrite what the user has already asked for.
        let (mut d, stats) = dash_with(vec![receiver(51, "Pool")]);

        d.on_key(KeyCode::Char('-'));
        d.on_key(KeyCode::Char('-'));
        assert_eq!(d.selected_receiver().unwrap().trim_db, -2.0 * TRIM_STEP_DB);

        // The stream hasn't caught up: it still reports 0.
        d.sample(&stats, Instant::now());
        assert_eq!(
            d.selected_receiver().unwrap().trim_db,
            -2.0 * TRIM_STEP_DB,
            "a stale sample must not undo the user's presses"
        );

        // Next press continues from where the user was, not from stale state.
        let action = d.on_key(KeyCode::Char('-'));
        assert_eq!(
            action,
            DashAction::Command(StreamCommand::SetTrim {
                addr: addr(51),
                db: -3.0 * TRIM_STEP_DB
            })
        );
    }

    #[test]
    fn the_stream_wins_once_it_confirms() {
        // The stream is the authority: once it reports the value back, its
        // number stands, so a trim it clamped or refused becomes visible.
        let (mut d, stats) = dash_with(vec![receiver(51, "Pool")]);
        d.on_key(KeyCode::Char('-'));

        let mut confirmed = receiver(51, "Pool");
        confirmed.trim_db = -TRIM_STEP_DB;
        stats.set_receivers(vec![confirmed]);
        d.sample(&stats, Instant::now());

        // Now a value the stream chose on its own must show through.
        let mut clamped = receiver(51, "Pool");
        clamped.trim_db = 0.0;
        stats.set_receivers(vec![clamped]);
        d.sample(&stats, Instant::now());
        assert_eq!(d.selected_receiver().unwrap().trim_db, 0.0);
    }

    #[test]
    fn angle_brackets_emit_an_offset_command() {
        let (mut d, _) = dash_with(vec![receiver(51, "Pool")]);
        assert_eq!(
            d.on_key(KeyCode::Char('>')),
            DashAction::Command(StreamCommand::SetOffset {
                addr: addr(51),
                ms: OFFSET_STEP_MS
            })
        );
    }

    #[test]
    fn offset_clamps_at_the_bounds() {
        let mut receivers = vec![receiver(51, "Pool")];
        receivers[0].offset_ms = OFFSET_MAX_MS;
        let (mut d, _) = dash_with(receivers);
        assert_eq!(
            d.on_key(KeyCode::Char('>')),
            DashAction::Command(StreamCommand::SetOffset {
                addr: addr(51),
                ms: OFFSET_MAX_MS
            })
        );
    }

    #[test]
    fn d_removes_the_selected_receiver() {
        let (mut d, _) = dash_with(vec![receiver(51, "Pool"), receiver(52, "Living")]);
        d.on_key(KeyCode::Down);
        assert_eq!(
            d.on_key(KeyCode::Char('d')),
            DashAction::Command(StreamCommand::Remove { addr: addr(52) })
        );
    }

    #[test]
    fn r_retries_a_failed_receiver() {
        let mut failed = receiver(51, "Pool");
        failed.state = ReceiverState::Failed;
        failed.error = Some("connection refused".into());
        let (mut d, _) = dash_with(vec![failed]);
        d.set_device_ids(HashMap::from([(addr(51), "AA:BB".to_string())]));

        assert_eq!(
            d.on_key(KeyCode::Char('r')),
            DashAction::Command(StreamCommand::Add {
                addr: addr(51),
                device_id: "AA:BB".into()
            })
        );
    }

    #[test]
    fn r_on_a_live_receiver_does_nothing() {
        // Retrying a working session would tear it down to rebuild it.
        let (mut d, _) = dash_with(vec![receiver(51, "Pool")]);
        assert_eq!(d.on_key(KeyCode::Char('r')), DashAction::None);
    }

    #[test]
    fn a_retry_without_a_known_device_id_falls_back_to_the_default() {
        let mut failed = receiver(51, "Pool");
        failed.state = ReceiverState::Failed;
        let (mut d, _) = dash_with(vec![failed]);

        assert_eq!(
            d.on_key(KeyCode::Char('r')),
            DashAction::Command(StreamCommand::Add {
                addr: addr(51),
                device_id: DEFAULT_DEVICE_ID.into()
            })
        );
    }

    #[test]
    fn a_failed_row_is_reachable_with_the_arrows() {
        let mut failed = receiver(52, "Living");
        failed.state = ReceiverState::Failed;
        let (mut d, _) = dash_with(vec![receiver(51, "Pool"), failed]);

        d.on_key(KeyCode::Down);
        assert_eq!(d.selected_receiver().unwrap().name, "Living");
    }

    #[test]
    fn a_opens_the_picker() {
        let (mut d, _) = dash_with(vec![receiver(51, "Pool")]);
        assert_eq!(d.on_key(KeyCode::Char('a')), DashAction::OpenPicker);
    }

    #[test]
    fn control_keys_are_inert_with_no_receivers() {
        // Every one of these dereferences the selection.
        let mut d = dash();
        for key in [
            KeyCode::Char('+'),
            KeyCode::Char('-'),
            KeyCode::Char('>'),
            KeyCode::Char('<'),
            KeyCode::Char('d'),
            KeyCode::Up,
            KeyCode::Down,
        ] {
            assert_eq!(d.on_key(key), DashAction::None, "{key:?} must be a no-op");
        }
    }

    #[test]
    fn page_keys_scroll_the_logs() {
        let mut d = dash();
        d.on_key(KeyCode::PageUp);
        assert_eq!(d.log_scroll, 1);
        d.on_key(KeyCode::PageDown);
        assert_eq!(d.log_scroll, 0);
        d.on_key(KeyCode::PageDown);
        assert_eq!(d.log_scroll, 0, "never negative");
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
            trim_db: 0.0,
            lead_ms: None,
            health: 0.0,
            error: None,
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
