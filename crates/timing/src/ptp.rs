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

/// Read a 10-byte PTP timestamp (u16 sec_hi, u32 sec_lo, u32 ns) as total ns.
fn read_ptp_timestamp(buf: &[u8]) -> u64 {
    let sec_hi = u16::from_be_bytes([buf[0], buf[1]]) as u64;
    let sec_lo = u32::from_be_bytes([buf[2], buf[3], buf[4], buf[5]]) as u64;
    let ns = u32::from_be_bytes([buf[6], buf[7], buf[8], buf[9]]) as u64;
    ((sec_hi << 32) | sec_lo) * 1_000_000_000 + ns
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
const MSG_DELAY_REQ: u8 = 0x1;
const MSG_FOLLOW_UP: u8 = 0x8;
const MSG_DELAY_RESP: u8 = 0x9;
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

/// Build a Delay_Resp for a received Delay_Req.
///
/// Apple's PTP slave (tvOS/HomePod) does full IEEE 1588 E2E delay
/// measurement: it sends Delay_Req to the master and will not consider its
/// clock calibrated until it gets Delay_Resp back. (nqptp never sends
/// Delay_Req, which is why Shairport receivers work without this.)
///
/// Layout: 34-byte header (type 0x9, length 54, control 0x03) ||
/// receiveTimestamp (10) || requestingPortIdentity (10, copied from the
/// request's sourcePortIdentity).
fn build_delay_resp(clock_id: &[u8; 8], req: &[u8], t4_ns: u64) -> Option<[u8; 54]> {
    if req.len() < 44 {
        return None;
    }
    let seq = u16::from_be_bytes([req[30], req[31]]);
    let mut m = [0u8; 54];
    m[..34].copy_from_slice(&header(MSG_DELAY_RESP, 54, 0x0000, clock_id, seq, 3));
    m[4] = req[4]; // mirror domainNumber
    m[8..16].copy_from_slice(&req[8..16]); // mirror correctionField
    m[33] = req[33]; // logMessageInterval
    write_ptp_timestamp(&mut m[34..44], t4_ns);
    m[44..54].copy_from_slice(&req[20..30]); // requestingPortIdentity
    Some(m)
}

/// A foreign PTP master we are tracking (e.g. an Apple TV's own clock).
///
/// Apple receivers run their own grandmaster and (at least tvOS) never
/// slave to a third-party sender's clock. The AirPlay 2 timing-group model
/// makes this legal: anchors must simply be expressed on the *elected*
/// grandmaster's timeline — which can be the receiver itself. So when a
/// foreign master is active we yield (BMCA-style) and translate our anchor
/// times onto its timeline instead.
#[derive(Debug, Clone, Copy)]
struct ForeignMaster {
    /// Grandmaster identity from its Announce messages.
    gm_id: u64,
    /// master_time ≈ local_time + offset_ns (from two-step Sync/Follow_Up;
    /// ignores path delay — sub-ms on a LAN, fine for anchor granularity).
    offset_ns: i64,
    last_seen: std::time::Instant,
    samples: u32,
}

#[derive(Default)]
struct SharedForeign(std::sync::Mutex<Option<ForeignMaster>>);

/// The timeline anchors should be expressed on right now.
#[derive(Debug, Clone, Copy)]
pub struct Timeline {
    /// Grandmaster clock ID for networkTimeTimelineID.
    pub gm_id: u64,
    /// Add to `ptp_now_ns()` to get time on that timeline.
    pub offset_ns: i64,
}

/// Handle to the running PTP master.
pub struct PtpMaster {
    pub clock_id: u64,
    foreign: Arc<SharedForeign>,
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

        let foreign = Arc::new(SharedForeign::default());

        // Local receive-times of the foreign master's Sync messages, keyed by
        // sequence number, matched against the origin timestamps that arrive
        // in its Follow_Up messages (two-step clock).
        let sync_rx_times: Arc<std::sync::Mutex<std::collections::HashMap<u16, u64>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

        // --- Receive path (Apple receivers need this; nqptp doesn't) ---
        // Event socket (319): the foreign master's Sync messages land here
        // (timestamp them for offset computation), and any Delay_Req gets an
        // immediate Delay_Resp in case a receiver does slave to us.
        {
            let rx_event = event.try_clone()?;
            let tx_general = general.try_clone()?;
            let stop_rx = stop.clone();
            let sync_times = sync_rx_times.clone();
            rx_event.set_read_timeout(Some(Duration::from_millis(250)))?;
            std::thread::spawn(move || {
                let mut buf = [0u8; 128];
                while !stop_rx.load(Ordering::Relaxed) {
                    let Ok((n, src)) = rx_event.recv_from(&mut buf) else {
                        continue; // timeout — recheck stop flag
                    };
                    let t_rx = ptp_now_ns();
                    if n < 34 || buf[1] & 0x0F != 0x02 {
                        continue;
                    }
                    match buf[0] & 0x0F {
                        MSG_SYNC => {
                            let seq = u16::from_be_bytes([buf[30], buf[31]]);
                            let mut g = sync_times.lock().unwrap();
                            g.insert(seq, t_rx);
                            if g.len() > 64 {
                                // Drop stale entries so the map stays bounded.
                                let newest = seq;
                                g.retain(|k, _| newest.wrapping_sub(*k) < 32);
                            }
                        }
                        MSG_DELAY_REQ => {
                            if let Some(resp) = build_delay_resp(&clock_bytes, &buf[..n], t_rx) {
                                let dest = SocketAddr::new(src.ip(), 320);
                                if let Err(e) = tx_general.send_to(&resp, dest) {
                                    warn!("delay_resp send failed: {e}");
                                } else {
                                    debug!(src = %src, "Delay_Req answered");
                                }
                            }
                        }
                        other => debug!(msg_type = other, src = %src, "PTP event rx (ignored)"),
                    }
                }
            });
        }
        // General socket (320): the foreign master's Announce (grandmaster
        // identity) and Follow_Up (Sync origin timestamps → clock offset).
        {
            let rx_general = general.try_clone()?;
            let stop_rx = stop.clone();
            let sync_times = sync_rx_times.clone();
            let foreign_rx = foreign.clone();
            rx_general.set_read_timeout(Some(Duration::from_millis(250)))?;
            std::thread::spawn(move || {
                let mut buf = [0u8; 128];
                while !stop_rx.load(Ordering::Relaxed) {
                    let Ok((n, src)) = rx_general.recv_from(&mut buf) else {
                        continue;
                    };
                    if n < 34 || buf[1] & 0x0F != 0x02 {
                        continue;
                    }
                    match buf[0] & 0x0F {
                        MSG_ANNOUNCE if n >= 64 => {
                            let gm_id = u64::from_be_bytes(buf[53..61].try_into().unwrap());
                            let mut g = foreign_rx.0.lock().unwrap();
                            match g.as_mut() {
                                Some(f) if f.gm_id == gm_id => {
                                    f.last_seen = std::time::Instant::now();
                                }
                                _ => {
                                    info!(
                                        gm = format!("{gm_id:016x}"), src = %src,
                                        "foreign PTP master detected — yielding (anchors will use its timeline)"
                                    );
                                    *g = Some(ForeignMaster {
                                        gm_id,
                                        offset_ns: 0,
                                        last_seen: std::time::Instant::now(),
                                        samples: 0,
                                    });
                                }
                            }
                        }
                        MSG_FOLLOW_UP if n >= 44 => {
                            let seq = u16::from_be_bytes([buf[30], buf[31]]);
                            let t_origin = read_ptp_timestamp(&buf[34..44]);
                            let rx_time = sync_times.lock().unwrap().remove(&seq);
                            if let Some(t_rx) = rx_time {
                                let sample = t_origin as i64 - t_rx as i64;
                                let mut g = foreign_rx.0.lock().unwrap();
                                if let Some(f) = g.as_mut() {
                                    // EWMA after the first sample; ~sub-ms
                                    // path delay is absorbed into the offset.
                                    f.offset_ns = if f.samples == 0 {
                                        sample
                                    } else {
                                        f.offset_ns + (sample - f.offset_ns) / 8
                                    };
                                    f.samples += 1;
                                    f.last_seen = std::time::Instant::now();
                                    if f.samples <= 3 || f.samples % 32 == 0 {
                                        debug!(
                                            offset_ms = f.offset_ns as f64 / 1e6,
                                            samples = f.samples,
                                            "foreign master offset updated"
                                        );
                                    }
                                }
                            }
                        }
                        other => debug!(msg_type = other, src = %src, "PTP general rx"),
                    }
                }
            });
        }

        let foreign_tx = foreign.clone();
        let thread = std::thread::spawn(move || {
            let mut announce_seq: u16 = 0;
            let mut sync_seq: u16 = 0;
            let mut yielding = false;
            let mut last_announce = std::time::Instant::now() - Duration::from_secs(2);
            loop {
                if stop2.load(Ordering::Relaxed) {
                    break;
                }
                // BMCA-style yield: while a foreign master is actively
                // announcing (Apple TV / HomePod run their own grandmaster
                // and won't slave to us), stop transmitting our own
                // Announce/Sync so we don't fight its clock. If it goes
                // quiet for >5s, resume mastering.
                let foreign_active = foreign_tx
                    .0
                    .lock()
                    .unwrap()
                    .map(|f| f.last_seen.elapsed() < Duration::from_secs(5))
                    .unwrap_or(false);
                if foreign_active != yielding {
                    yielding = foreign_active;
                    info!(yielding, "PTP master role changed");
                }
                if !yielding {
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
                }
                // Fast cadence for the first ~3s: the receiver's clock
                // daemon (nqptp) resets its records when a session starts
                // and its offset smoothing needs several follow_ups to
                // converge — more early samples = less audible resync/mute
                // churn at stream start.
                let interval = if sync_seq < 30 { 100 } else { 250 };
                std::thread::sleep(Duration::from_millis(interval));
            }
        });

        Ok(PtpMaster { clock_id, foreign, stop, thread: Some(thread) })
    }

    /// The timeline anchors must be expressed on right now.
    ///
    /// If a foreign master (receiver-side grandmaster) is active with a
    /// converged offset, use its clock ID and offset; otherwise our own
    /// clock (offset 0).
    pub fn active_timeline(&self) -> Timeline {
        let g = self.foreign.0.lock().unwrap();
        if let Some(f) = g.as_ref() {
            if f.last_seen.elapsed() < Duration::from_secs(5) && f.samples >= 3 {
                return Timeline { gm_id: f.gm_id, offset_ns: f.offset_ns };
            }
        }
        Timeline { gm_id: self.clock_id, offset_ns: 0 }
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
    fn delay_resp_mirrors_request() {
        let our_id = [0xAAu8; 8];
        // Fake Delay_Req: type 1, from clock 0x0102...08 port 1, seq 777
        let mut req = [0u8; 44];
        req[0] = MSG_DELAY_REQ;
        req[1] = 0x02;
        req[4] = 0; // domain
        req[8..16].copy_from_slice(&[0, 0, 0, 0, 0, 0, 0x12, 0x34]); // correction
        req[20..28].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        req[28..30].copy_from_slice(&1u16.to_be_bytes());
        req[30..32].copy_from_slice(&777u16.to_be_bytes());
        req[33] = 0x7F;

        let resp = build_delay_resp(&our_id, &req, 5_000_000_003).unwrap();
        assert_eq!(resp[0] & 0x0F, MSG_DELAY_RESP);
        assert_eq!(u16::from_be_bytes([resp[2], resp[3]]), 54);
        assert_eq!(u16::from_be_bytes([resp[30], resp[31]]), 777); // seq mirrored
        assert_eq!(resp[32], 3); // control = Delay_Resp
        assert_eq!(resp[33], 0x7F); // logMessageInterval mirrored
        assert_eq!(&resp[8..16], &req[8..16]); // correction mirrored
        assert_eq!(&resp[20..28], &our_id); // our identity as sender
        assert_eq!(&resp[44..54], &req[20..30]); // requestingPortIdentity
        // receiveTimestamp = 5s + 3ns
        assert_eq!(u32::from_be_bytes([resp[36], resp[37], resp[38], resp[39]]), 5);
        assert_eq!(u32::from_be_bytes([resp[40], resp[41], resp[42], resp[43]]), 3);
        // Truncated request → None
        assert!(build_delay_resp(&our_id, &req[..30], 0).is_none());
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
