//! NTP-style timing for single-room AirPlay 2 realtime streams (Step 4).
//!
//! The receiver sends timing requests (PT=0x52) to our UDP timing port; we
//! reply (PT=0x53) with its send time echoed as "reference" plus our
//! receive/send times in NTP format (epoch 1900).
//!
//! PTP lives in the `ptp` module (pulled forward from Step 6 since both
//! Shairport Sync and Apple TV require it; Shairport cannot do AP2-NTP).
pub mod ptp;
pub use ptp::{ptp_now_ns, ptp_ns_to_secs_frac, PtpMaster, Timeline};

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{debug, warn};

/// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
pub const NTP_UNIX_OFFSET: u64 = 0x83AA_7E80;

/// Current time in 64-bit NTP format: seconds (high 32) | fraction (low 32).
pub fn ntp_now() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() + NTP_UNIX_OFFSET;
    let micros = u64::from(now.subsec_micros());
    (secs << 32) | ((micros << 32) / 1_000_000)
}

/// Convert NTP time to an RTP timestamp at `rate` Hz (pyatv `ntp2ts`).
pub fn ntp_to_ts(ntp: u64, rate: u32) -> u64 {
    ((ntp >> 16) * u64::from(rate)) >> 16
}

/// Convert an RTP timestamp at `rate` Hz to NTP time (pyatv `ts2ntp`).
pub fn ts_to_ntp(ts: u64, rate: u32) -> u64 {
    ((ts << 16) / u64::from(rate)) << 16
}

/// UDP responder for NTP-style timing requests.
///
/// Request/reply layout (32 bytes, all fields big-endian):
/// ```text
/// u8 proto, u8 type, u16 seqno, u32 padding,
/// u32 reftime_sec,  u32 reftime_frac,   // reply: echo of request send time
/// u32 recvtime_sec, u32 recvtime_frac,  // reply: our receive time
/// u32 sendtime_sec, u32 sendtime_frac   // reply: our send time
/// ```
pub struct TimingResponder {
    socket: UdpSocket,
    stop: Arc<AtomicBool>,
    pub port: u16,
}

impl TimingResponder {
    /// Bind an ephemeral UDP port and return the responder (not yet running).
    pub fn bind() -> std::io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", 0))?;
        let port = socket.local_addr()?.port();
        Ok(TimingResponder {
            socket,
            stop: Arc::new(AtomicBool::new(false)),
            port,
        })
    }

    /// Spawn the reply loop on a background thread. Returns a stop handle.
    pub fn spawn(self) -> TimingHandle {
        let stop = self.stop.clone();
        let socket = self.socket;
        socket
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .ok();
        let handle = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let (len, peer) = match socket.recv_from(&mut buf) {
                    Ok(x) => x,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue
                    }
                    Err(e) => {
                        warn!("timing socket error: {e}");
                        break;
                    }
                };
                if len < 32 {
                    continue;
                }
                let now = ntp_now();
                let mut resp = [0u8; 32];
                resp[0] = buf[0]; // proto (0x80)
                resp[1] = 0x53 | 0x80; // timing reply
                resp[2..4].copy_from_slice(&7u16.to_be_bytes());
                // padding stays zero
                // reftime = request's sendtime (bytes 24..32)
                resp[8..16].copy_from_slice(&buf[24..32]);
                let sec = (now >> 32) as u32;
                let frac = (now & 0xFFFF_FFFF) as u32;
                resp[16..20].copy_from_slice(&sec.to_be_bytes());
                resp[20..24].copy_from_slice(&frac.to_be_bytes());
                resp[24..28].copy_from_slice(&sec.to_be_bytes());
                resp[28..32].copy_from_slice(&frac.to_be_bytes());
                if let Err(e) = socket.send_to(&resp, peer) {
                    warn!("timing reply failed: {e}");
                } else {
                    debug!(peer = %peer, "timing reply sent");
                }
            }
        });
        TimingHandle { stop: self.stop, thread: Some(handle) }
    }
}

/// Handle to stop the timing responder thread.
pub struct TimingHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for TimingHandle {
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
    fn ntp_epoch_offset() {
        let ntp = ntp_now();
        let secs = ntp >> 32;
        // Must be > offset (we are past 1970) and in a sane range (< year 2100)
        assert!(secs > NTP_UNIX_OFFSET);
        assert!(secs < NTP_UNIX_OFFSET + 4_102_444_800);
    }

    #[test]
    fn ts_ntp_roundtrip() {
        let ts = 123_456_789u64;
        let ntp = ts_to_ntp(ts, 44100);
        let back = ntp_to_ts(ntp, 44100);
        // Conversion truncates fractions; allow 1 sample slack
        assert!(ts.abs_diff(back) <= 1, "{ts} vs {back}");
    }

    #[test]
    fn responder_replies_to_requests() {
        let responder = TimingResponder::bind().unwrap();
        let port = responder.port;
        let _handle = responder.spawn();

        let client = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        client
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        let mut req = [0u8; 32];
        req[0] = 0x80;
        req[1] = 0x52;
        req[24..28].copy_from_slice(&0xAABBCCDDu32.to_be_bytes());
        req[28..32].copy_from_slice(&0x11223344u32.to_be_bytes());
        client.send_to(&req, ("127.0.0.1", port)).unwrap();

        let mut buf = [0u8; 64];
        let (len, _) = client.recv_from(&mut buf).unwrap();
        assert_eq!(len, 32);
        assert_eq!(buf[1], 0xD3); // 0x53 | 0x80
        // reftime must echo our sendtime
        assert_eq!(&buf[8..12], &0xAABBCCDDu32.to_be_bytes());
        assert_eq!(&buf[12..16], &0x11223344u32.to_be_bytes());
        // recvtime == sendtime (same instant), nonzero
        assert_eq!(&buf[16..24], &buf[24..32]);
        assert_ne!(&buf[16..24], &[0u8; 8]);
    }
}
