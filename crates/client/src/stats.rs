//! A live view of a running buffered stream, for observers such as the TUI
//! dashboard.
//!
//! The design rule: **numbers by snapshot, events by tracing.** Every field
//! here is a *current value*, never a history — an observer samples at whatever
//! rate it renders and keeps its own history. Discrete events (underrun,
//! receiver dropped, reconnect succeeded) are already `warn!`/`info!` lines and
//! reach the log panel through the tracing layer, so they are deliberately
//! absent here.
//!
//! That split is what keeps this struct from growing: it never has to buffer
//! anything, and a stalled renderer cannot apply backpressure to audio.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use openair_core::metadata::NowPlaying;

/// Sentinel for "no sample since the last read". `i64::MAX` is safe because a
/// real lead is bounded by the anchor latency.
const NO_SAMPLE: i64 = i64::MAX;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiverState {
    Connected,
    /// Dropped, with a background reconnect in flight.
    Reconnecting,
    /// Gone and not coming back (file playback, or reconnect disabled).
    Dead,
}

impl ReceiverState {
    pub fn label(self) -> &'static str {
        match self {
            ReceiverState::Connected => "connected",
            ReceiverState::Reconnecting => "reconnecting…",
            ReceiverState::Dead => "dead",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReceiverStat {
    pub name: String,
    pub addr: SocketAddr,
    pub state: ReceiverState,
    pub offset_ms: i64,
    /// Per-receiver volume trim in dB, relative to the group's master level.
    pub trim_db: f32,
}

/// A request from an observer into a running stream.
///
/// Receivers are addressed by `addr` rather than by list index: the group's
/// membership changes underneath the UI as receivers drop, reconnect and get
/// added, so an index captured when a key was pressed can refer to a different
/// receiver by the time the loop drains it.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamCommand {
    /// Set a receiver's volume trim, in dB relative to the master level.
    SetTrim { addr: SocketAddr, db: f32 },
    /// Set a receiver's play offset in milliseconds; re-anchors that receiver.
    SetOffset { addr: SocketAddr, ms: i64 },
    /// Bring a new receiver into the group mid-stream.
    Add {
        addr: SocketAddr,
        device_id: String,
    },
    /// Remove a receiver: tear it down and do not reconnect.
    Remove { addr: SocketAddr },
}

/// Volume trim bounds. Wide enough to balance a quiet room against a loud one,
/// narrow enough that a held key can't silence a receiver by accident.
pub const TRIM_MIN_DB: f32 = -30.0;
pub const TRIM_MAX_DB: f32 = 10.0;

/// A receiver's effective volume: the group master plus its own trim, clamped
/// to the protocol's usable range (`-144` is AirPlay's "muted" sentinel).
///
/// Exposed so a UI can show the level a key press will actually produce rather
/// than guessing at the clamping.
pub fn effective_volume_db(master_db: f32, trim_db: f32) -> f32 {
    (master_db + trim_db).clamp(-144.0, 0.0)
}

pub struct StreamStats {
    /// Current anchor lead in ms, as raised by auto-latency.
    latency_ms: AtomicU64,
    /// Smallest headroom seen since the last read, in ms. May be negative.
    min_lead_ms: AtomicI64,
    /// Total payload bytes sent, monotonic. Readers difference it over time to
    /// get a rate, so the stream never needs to know the sampling interval.
    bytes_sent: AtomicU64,
    /// Set once the stream loop has returned.
    ended: AtomicBool,
    started_at: Instant,
    receivers: Mutex<Vec<ReceiverStat>>,
    now_playing: Mutex<Option<NowPlaying>>,
    /// Commands awaiting the stream loop, oldest first. Drained once per
    /// packet — about every 23 ms, so a keystroke lands well inside a frame.
    inbox: Mutex<Vec<StreamCommand>>,
}

impl StreamStats {
    pub fn new(latency_ms: u64) -> Arc<Self> {
        Arc::new(Self {
            latency_ms: AtomicU64::new(latency_ms),
            min_lead_ms: AtomicI64::new(NO_SAMPLE),
            bytes_sent: AtomicU64::new(0),
            ended: AtomicBool::new(false),
            started_at: Instant::now(),
            receivers: Mutex::new(Vec::new()),
            now_playing: Mutex::new(None),
            inbox: Mutex::new(Vec::new()),
        })
    }

    /// Queue a command for the stream loop. Returns `false` if the mailbox is
    /// unavailable, so a UI can say so rather than pretend the key worked.
    pub fn send(&self, cmd: StreamCommand) -> bool {
        match self.inbox.lock() {
            Ok(mut inbox) => {
                inbox.push(cmd);
                true
            }
            Err(_) => false,
        }
    }

    /// Take everything queued. Called by the stream loop.
    pub fn drain_commands(&self) -> Vec<StreamCommand> {
        match self.inbox.lock() {
            Ok(mut inbox) => std::mem::take(&mut *inbox),
            Err(_) => Vec::new(),
        }
    }

    // --- written by the stream loop ---

    pub fn set_latency_ms(&self, ms: u64) {
        self.latency_ms.store(ms, Ordering::Relaxed);
    }

    /// Record one headroom sample. Only the window minimum is kept, so a dip
    /// between two reads is never lost to averaging — the worst case is what
    /// predicts a dropout.
    pub fn record_lead_ms(&self, ms: i64) {
        self.min_lead_ms.fetch_min(ms, Ordering::Relaxed);
    }

    pub fn add_bytes(&self, n: u64) {
        self.bytes_sent.fetch_add(n, Ordering::Relaxed);
    }

    pub fn set_receivers(&self, receivers: Vec<ReceiverStat>) {
        if let Ok(mut slot) = self.receivers.lock() {
            *slot = receivers;
        }
    }

    pub fn set_now_playing(&self, np: NowPlaying) {
        if let Ok(mut slot) = self.now_playing.lock() {
            *slot = Some(np);
        }
    }

    pub fn mark_ended(&self) {
        self.ended.store(true, Ordering::Relaxed);
    }

    // --- read by observers ---

    pub fn latency_ms(&self) -> u64 {
        self.latency_ms.load(Ordering::Relaxed)
    }

    /// The minimum headroom since the previous call, rearming for the next
    /// window. `None` when no packet was sent in between.
    pub fn take_min_lead_ms(&self) -> Option<i64> {
        match self.min_lead_ms.swap(NO_SAMPLE, Ordering::Relaxed) {
            NO_SAMPLE => None,
            v => Some(v),
        }
    }

    pub fn bytes_sent(&self) -> u64 {
        self.bytes_sent.load(Ordering::Relaxed)
    }

    pub fn elapsed(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    pub fn ended(&self) -> bool {
        self.ended.load(Ordering::Relaxed)
    }

    pub fn receivers(&self) -> Vec<ReceiverStat> {
        self.receivers.lock().map(|r| r.clone()).unwrap_or_default()
    }

    pub fn now_playing(&self) -> Option<NowPlaying> {
        self.now_playing.lock().ok().and_then(|np| np.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_lead_is_none_before_any_sample() {
        let stats = StreamStats::new(500);
        assert_eq!(stats.take_min_lead_ms(), None);
    }

    #[test]
    fn min_lead_returns_the_window_minimum_and_rearms() {
        let stats = StreamStats::new(500);
        stats.record_lead_ms(400);
        stats.record_lead_ms(180);
        stats.record_lead_ms(350);
        assert_eq!(
            stats.take_min_lead_ms(),
            Some(180),
            "the dip must survive, not be averaged away"
        );
        assert_eq!(stats.take_min_lead_ms(), None, "rearmed for the next window");

        stats.record_lead_ms(500);
        assert_eq!(stats.take_min_lead_ms(), Some(500));
    }

    #[test]
    fn min_lead_handles_negative_headroom() {
        // A late packet is exactly what we most want to see.
        let stats = StreamStats::new(500);
        stats.record_lead_ms(120);
        stats.record_lead_ms(-30);
        assert_eq!(stats.take_min_lead_ms(), Some(-30));
    }

    #[test]
    fn bytes_accumulate_monotonically() {
        let stats = StreamStats::new(500);
        assert_eq!(stats.bytes_sent(), 0);
        stats.add_bytes(1024);
        stats.add_bytes(512);
        assert_eq!(stats.bytes_sent(), 1536);
    }

    #[test]
    fn latency_reflects_the_last_write() {
        let stats = StreamStats::new(500);
        assert_eq!(stats.latency_ms(), 500);
        stats.set_latency_ms(750);
        assert_eq!(stats.latency_ms(), 750);
    }

    #[test]
    fn receivers_round_trip() {
        let stats = StreamStats::new(500);
        assert!(stats.receivers().is_empty());
        stats.set_receivers(vec![ReceiverStat {
            name: "Pool Room".into(),
            addr: "192.168.1.51:7000".parse().unwrap(),
            state: ReceiverState::Reconnecting,
            offset_ms: 80,
            trim_db: 0.0,
        }]);
        let got = stats.receivers();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].state, ReceiverState::Reconnecting);
        assert_eq!(got[0].name, "Pool Room");
    }

    #[test]
    fn trim_shifts_the_master_level() {
        assert_eq!(effective_volume_db(-8.0, 0.0), -8.0);
        assert_eq!(effective_volume_db(-8.0, -6.0), -14.0);
    }

    #[test]
    fn moving_the_master_preserves_the_balance_between_receivers() {
        // The reason trims exist rather than absolute per-receiver levels:
        // --handoff moves the master on every Windows volume change, and the
        // gap the user dialled in has to survive that.
        let (quiet, loud) = (-6.0, 0.0);
        let before = effective_volume_db(-8.0, quiet) - effective_volume_db(-8.0, loud);
        let after = effective_volume_db(-20.0, quiet) - effective_volume_db(-20.0, loud);
        assert_eq!(before, after);
    }

    #[test]
    fn effective_volume_never_exceeds_full_scale() {
        // A positive trim on an already-loud master must not overdrive.
        assert_eq!(effective_volume_db(-2.0, 10.0), 0.0);
    }

    #[test]
    fn a_deep_trim_mutes_rather_than_wrapping() {
        assert_eq!(effective_volume_db(-140.0, -30.0), -144.0);
    }

    #[test]
    fn commands_queue_and_drain_in_order() {
        let stats = StreamStats::new(500);
        let a: SocketAddr = "192.168.1.51:7000".parse().unwrap();
        let b: SocketAddr = "192.168.1.52:7000".parse().unwrap();
        assert!(stats.send(StreamCommand::SetTrim { addr: a, db: -3.0 }));
        assert!(stats.send(StreamCommand::Remove { addr: b }));

        let drained = stats.drain_commands();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0], StreamCommand::SetTrim { addr: a, db: -3.0 });
        assert_eq!(drained[1], StreamCommand::Remove { addr: b });
    }

    #[test]
    fn draining_empties_the_inbox() {
        let stats = StreamStats::new(500);
        let a: SocketAddr = "192.168.1.51:7000".parse().unwrap();
        stats.send(StreamCommand::SetTrim { addr: a, db: 2.0 });
        assert_eq!(stats.drain_commands().len(), 1);
        assert!(
            stats.drain_commands().is_empty(),
            "a command must not be applied twice"
        );
    }

    #[test]
    fn commands_survive_being_sent_from_another_thread() {
        let stats = StreamStats::new(500);
        let sender = Arc::clone(&stats);
        let addr: SocketAddr = "192.168.1.51:7000".parse().unwrap();
        std::thread::spawn(move || {
            for i in 0..50 {
                sender.send(StreamCommand::SetTrim {
                    addr,
                    db: i as f32,
                });
            }
        })
        .join()
        .unwrap();
        assert_eq!(stats.drain_commands().len(), 50);
    }

    #[test]
    fn ended_starts_false_and_latches() {
        let stats = StreamStats::new(500);
        assert!(!stats.ended());
        stats.mark_ended();
        assert!(stats.ended());
    }

    #[test]
    fn shared_across_threads() {
        // The whole point: the stream loop writes on its thread, the renderer
        // reads on another.
        let stats = StreamStats::new(500);
        let writer = Arc::clone(&stats);
        let t = std::thread::spawn(move || {
            for _ in 0..1000 {
                writer.add_bytes(1);
                writer.record_lead_ms(42);
            }
        });
        t.join().unwrap();
        assert_eq!(stats.bytes_sent(), 1000);
        assert_eq!(stats.take_min_lead_ms(), Some(42));
    }
}
