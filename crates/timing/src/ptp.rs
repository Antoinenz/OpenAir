//! Minimal IEEE 1588 PTP *master* — just enough for AirPlay 2 receivers.
//!
//! Shairport Sync's companion daemon `nqptp` is listen-only: it qualifies a
//! clock from Announce messages and derives offsets from two-step
//! Sync + Follow_Up pairs. No BMCA, no Delay_Req handling needed (that comes
//! with HomePod in Step 6's full implementation).
//!
//! Ports 319 (event: Sync) and 320 (general: Announce, Follow_Up), unicast to
//! the receiver. On Windows these ports need no elevation; on Linux this will
//! move behind the ptp-helper binary.
use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing::{debug, info, warn};

/// Our PTP clock time: nanoseconds since the Unix epoch.
/// (The epoch is arbitrary as long as anchor times use the same timeline.)
pub fn ptp_now_ns() -> u64 {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    now.as_secs() * 1_000_000_000 + u64::from(now.subsec_nanos())
}

/// Split PTP nanoseconds into (seconds, 2^-64 second fraction) for
/// SETRATEANCHORTIME's networkTimeSecs/networkTimeFrac.
pub fn ptp_ns_to_secs_frac(ns: u64) -> (u64, u64) {
    let secs = ns / 1_000_000_000;
    let rem_ns = ns % 1_000_000_000;
    // frac = rem_ns / 1e9 * 2^64, computed as ((rem << 32) / 1e9) << 32
    let frac = ((rem_ns << 32) / 1_000_000_000) << 32;
    (secs, frac)
}

/// Write a 10-byte PTP timestamp (u16 sec_hi, u32 sec_lo, u32 ns) into `buf`.
fn write_ptp_timestamp(buf: &mut [u8], ns_total: u64) {
    let secs = ns_total / 1_000_000_000;
    let ns = (ns_total % 1_000_000_000) as u32;
    buf[0..2].copy_from_slice(&((secs >> 32) as u16).to_be_bytes());
    buf[2..6].copy_from_slice(&((secs & 0xFFFF_FFFF) as u32).to_be_bytes());
    buf[6..10].copy_from_slice(&ns.to_be_bytes());
}

const MSG_SYNC: u8 = 0x0;
const MSG_FOLLOW_UP: u8 = 0x8;
const MSG_ANNOUNCE: u8 = 0xB;

fn header(msg_type: u8, length: u16, flags: u16, clock_id: &[u8; 8], seq: u16, control: u8) -> [u8; 34] {
    let mut h = [0u8; 34];
    h[0] = msg_type; // transportSpecific = 0
    h[1] = 0x02; // PTPv2
    h[2..4].copy_from_slice(&length.to_be_bytes());
    // domain 0, reserved
    h[6..8].copy_from_slice(&flags.to_be_bytes());
    // correctionField + reserved stay zero
    h[20..28].copy_from_slice(clock_id);
    h[28..30].copy_from_slice(&1u16.to_be_bytes()); // sourcePortID
    h[30..32].copy_from_slice(&seq.to_be_bytes());
    h[32] = control;
    h[33] = 0; // logMessagePeriod
    h
}

fn build_announce(clock_id: &[u8; 8], seq: u16) -> [u8; 64] {
    let mut m = [0u8; 64];
    m[..34].copy_from_slice(&header(MSG_ANNOUNCE, 64, 0x0000, clock_id, seq, 5));
    // originTimestamp zeros (34..44)
    m[44..46].copy_from_slice(&37u16.to_be_bytes()); // currentUtcOffset
    m[47] = 248; // grandmasterPriority1
    m[48..52].copy_from_slice(&0xF8FE_FFFFu32.to_be_bytes()); // clockQuality
    m[52] = 248; // grandmasterPriority2
    m[53..61].copy_from_slice(clock_id); // grandmasterIdentity
    // stepsRemoved 0
    m[63] = 0xA0; // timeSource: internal oscillator
    m
}

fn build_sync(clock_id: &[u8; 8], seq: u16) -> [u8; 44] {
    let mut m = [0u8; 44];
    // two-step flag set — real timestamp follows in Follow_Up
    m[..34].copy_from_slice(&header(MSG_SYNC, 44, 0x0200, clock_id, seq, 0));
    m
}

fn build_follow_up(clock_id: &[u8; 8], seq: u16, sync_time_ns: u64) -> [u8; 44] {
    let mut m = [0u8; 44];
    m[..34].copy_from_slice(&header(MSG_FOLLOW_UP, 44, 0x0000, clock_id, seq, 2));
    write_ptp_timestamp(&mut m[34..44], sync_time_ns);
    m
}

/// Handle to the running PTP master.
pub struct PtpMaster {
    pub clock_id: u64,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PtpMaster {
    /// Start announcing + syncing to `peer` (receiver IP).
    ///
    /// Binds UDP 319/320 (fails without privileges on Linux/macOS — fine on
    /// Windows). Announce 1 Hz, Sync/Follow_Up 4 Hz.
    pub fn start(peer: IpAddr) -> std::io::Result<Self> {
        let event = UdpSocket::bind(("0.0.0.0", 319))?;
        let general = UdpSocket::bind(("0.0.0.0", 320))?;
        let event_dest = SocketAddr::new(peer, 319);
        let general_dest = SocketAddr::new(peer, 320);

        let mut clock_bytes = [0u8; 8];
        // Derive a stable-ish clock identity from process randomness.
        let seed = ptp_now_ns() ^ (std::process::id() as u64).rotate_left(32);
        clock_bytes.copy_from_slice(&seed.to_be_bytes());
        let clock_id = u64::from_be_bytes(clock_bytes);

        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        info!(clock_id = format!("{clock_id:016x}"), peer = %peer, "PTP master starting");

        let thread = std::thread::spawn(move || {
            let mut announce_seq: u16 = 0;
            let mut sync_seq: u16 = 0;
            let mut last_announce = std::time::Instant::now() - Duration::from_secs(2);
            loop {
                if stop2.load(Ordering::Relaxed) {
                    break;
                }
                if last_announce.elapsed() >= Duration::from_secs(1) {
                    let a = build_announce(&clock_bytes, announce_seq);
                    if let Err(e) = general.send_to(&a, general_dest) {
                        warn!("announce send failed: {e}");
                    }
                    announce_seq = announce_seq.wrapping_add(1);
                    last_announce = std::time::Instant::now();
                }
                let s = build_sync(&clock_bytes, sync_seq);
                let t = ptp_now_ns();
                if let Err(e) = event.send_to(&s, event_dest) {
                    warn!("sync send failed: {e}");
                }
                let f = build_follow_up(&clock_bytes, sync_seq, t);
                if let Err(e) = general.send_to(&f, general_dest) {
                    warn!("follow_up send failed: {e}");
                }
                debug!(seq = sync_seq, "PTP sync+follow_up sent");
                sync_seq = sync_seq.wrapping_add(1);
                // Fast cadence for the first ~3s: the receiver's clock
                // daemon (nqptp) resets its records when a session starts
                // and its offset smoothing needs several follow_ups to
                // converge — more early samples = less audible resync/mute
                // churn at stream start.
                let interval = if sync_seq < 30 { 100 } else { 250 };
                std::thread::sleep(Duration::from_millis(interval));
            }
        });

        Ok(PtpMaster { clock_id, stop, thread: Some(thread) })
    }
}

impl Drop for PtpMaster {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_layout() {
        let mut buf = [0u8; 10];
        // 0x1_00000002 seconds + 3ns
        let ns = ((1u64 << 32) + 2) * 1_000_000_000 + 3;
        write_ptp_timestamp(&mut buf, ns);
        assert_eq!(&buf[0..2], &1u16.to_be_bytes());
        assert_eq!(&buf[2..6], &2u32.to_be_bytes());
        assert_eq!(&buf[6..10], &3u32.to_be_bytes());
    }

    #[test]
    fn secs_frac_conversion() {
        let ns = 5 * 1_000_000_000 + 500_000_000; // 5.5s
        let (secs, frac) = ptp_ns_to_secs_frac(ns);
        assert_eq!(secs, 5);
        // 0.5 as a 2^-64 fraction ≈ 0x8000...0; shairport recovers ns via
        // ((frac >> 32) * 1e9) >> 32 — verify roundtrip within 1ns
        let recovered = ((frac >> 32) * 1_000_000_000) >> 32;
        assert!(recovered.abs_diff(500_000_000) <= 1, "{recovered}");
    }

    #[test]
    fn announce_message_fields() {
        let id = [1, 2, 3, 4, 5, 6, 7, 8];
        let a = build_announce(&id, 42);
        assert_eq!(a.len(), 64);
        assert_eq!(a[0] & 0x0F, 0x0B);
        assert_eq!(a[1], 2);
        assert_eq!(u16::from_be_bytes([a[2], a[3]]), 64);
        assert_eq!(&a[20..28], &id);
        assert_eq!(u16::from_be_bytes([a[30], a[31]]), 42);
        assert_eq!(a[47], 248); // priority1
        assert_eq!(&a[53..61], &id); // grandmasterIdentity
    }

    #[test]
    fn sync_has_two_step_flag() {
        let id = [9u8; 8];
        let s = build_sync(&id, 1);
        assert_eq!(s[0] & 0x0F, 0x00);
        assert_eq!(u16::from_be_bytes([s[6], s[7]]), 0x0200);
        let f = build_follow_up(&id, 1, 1_500_000_000);
        assert_eq!(f[0] & 0x0F, 0x08);
        assert_eq!(u32::from_be_bytes([f[40], f[41], f[42], f[43]]), 500_000_000);
    }
}
