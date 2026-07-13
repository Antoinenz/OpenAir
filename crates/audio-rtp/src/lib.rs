//! RTP packetization + AEAD for AirPlay 2 realtime audio (PT=96), the control
//! channel (sync packets, retransmit replies) and packet backlog.
//!
//! Wire format (verified against pyatv's AirPlayV2 sender):
//! ```text
//! RTP header (12) || ciphertext || Poly1305 tag (16) || nonce (8)
//! ```
//! - Key: `shk` (sender-generated, sent in SETUP phase 2)
//! - Nonce: 12 bytes internally, low 8 bytes = little-endian counter,
//!   only those 8 bytes travel in the packet
//! - AAD: RTP header bytes 4..12 (timestamp BE || SSRC BE)
use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use tracing::{debug, warn};

/// Per-stream AEAD cipher with a monotonically increasing 8-byte nonce.
pub struct AudioCipher {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl AudioCipher {
    pub fn new(shk: &[u8; 32]) -> Self {
        AudioCipher {
            cipher: ChaCha20Poly1305::new(Key::from_slice(shk)),
            counter: 0,
        }
    }

    /// Encrypt `payload` with `aad`; returns (ciphertext||tag, 8-byte nonce).
    pub fn encrypt(&mut self, payload: &[u8], aad: &[u8]) -> (Vec<u8>, [u8; 8]) {
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&self.counter.to_le_bytes());
        self.counter += 1;
        let ct = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: payload, aad })
            .expect("chacha20poly1305 encrypt cannot fail");
        let mut n8 = [0u8; 8];
        n8.copy_from_slice(&nonce[4..]);
        (ct, n8)
    }
}

/// Build one encrypted realtime audio packet (PT=0x60, marker on first).
pub fn build_audio_packet(
    cipher: &mut AudioCipher,
    first: bool,
    seq: u16,
    timestamp: u32,
    ssrc: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut header = [0u8; 12];
    header[0] = 0x80;
    header[1] = if first { 0xE0 } else { 0x60 };
    header[2..4].copy_from_slice(&seq.to_be_bytes());
    header[4..8].copy_from_slice(&timestamp.to_be_bytes());
    header[8..12].copy_from_slice(&ssrc.to_be_bytes());

    let (ct, nonce8) = cipher.encrypt(payload, &header[4..12]);

    let mut packet = Vec::with_capacity(12 + ct.len() + 8);
    packet.extend_from_slice(&header);
    packet.extend_from_slice(&ct);
    packet.extend_from_slice(&nonce8);
    packet
}

/// SSRC for buffered AAC/44100/2 streams (type 103) — shairport-sync's
/// `AAC_44100_F24_2`.
pub const AAC_44100_F24_2_SSRC: u32 = 0x1600_0000;

/// Build one encrypted buffered-audio TCP block (stream type 103, AAC-LC).
///
/// Wire layout, written in full by this function (ready to write to the TCP
/// socket as-is):
/// ```text
/// u16 BE length            (includes these 2 bytes)
/// u32 BE 0x80800000 | (seq & 0x7FFFFF)   (version/marker byte + 23-bit seq)
/// u32 BE rtptime
/// u32 BE ssrc               (0x16000000 for AAC_44100_F24_2)
/// ciphertext || 16-byte tag
/// 8-byte nonce
/// ```
/// `payload` is one raw AAC-LC frame (no ADTS header — the receiver adds
/// it). AAD is the 8 bytes `rtptime || ssrc` (block bytes [4..12]).
pub fn build_buffered_audio_block(
    cipher: &mut AudioCipher,
    seq: u32,
    rtptime: u32,
    ssrc: u32,
    payload: &[u8],
) -> Vec<u8> {
    let mut prefix = [0u8; 12];
    let word0 = 0x8080_0000u32 | (seq & 0x007F_FFFF);
    prefix[0..4].copy_from_slice(&word0.to_be_bytes());
    prefix[4..8].copy_from_slice(&rtptime.to_be_bytes());
    prefix[8..12].copy_from_slice(&ssrc.to_be_bytes());

    let (ct, nonce8) = cipher.encrypt(payload, &prefix[4..12]);

    let body_len = 12 + ct.len() + 8;
    let total_len = 2 + body_len;
    let mut block = Vec::with_capacity(total_len);
    block.extend_from_slice(&(total_len as u16).to_be_bytes());
    block.extend_from_slice(&prefix);
    block.extend_from_slice(&ct);
    block.extend_from_slice(&nonce8);
    block
}

/// Build a control-channel sync packet (PT=0xD4, marker on first).
///
/// `rtptime` is the current stream head (with latency), `latency` in samples,
/// `ntp` the NTP time corresponding to the stream head.
pub fn build_sync_packet(first: bool, rtptime: u32, latency: u32, ntp: u64) -> [u8; 20] {
    let mut p = [0u8; 20];
    p[0] = if first { 0x90 } else { 0x80 };
    p[1] = 0xD4;
    p[2..4].copy_from_slice(&0x0007u16.to_be_bytes());
    p[4..8].copy_from_slice(&rtptime.wrapping_sub(latency).to_be_bytes());
    p[8..12].copy_from_slice(&(((ntp >> 32) & 0xFFFF_FFFF) as u32).to_be_bytes());
    p[12..16].copy_from_slice(&((ntp & 0xFFFF_FFFF) as u32).to_be_bytes());
    p[16..20].copy_from_slice(&rtptime.to_be_bytes());
    p
}

/// Realtime latency in samples announced in PTP anchor packets.
/// Shairport expects `frame_2 - frame_1 == 77175` (else it logs a warning).
pub const PTP_ANCHOR_LATENCY: u32 = 77_175;

/// Build an AP2 realtime "anchoring announcement" control packet (type 215).
///
/// Layout (28 bytes, from shairport-sync rtp_ap2_control_receiver):
/// ```text
/// [0]      0x80 (| 0x10 sentinel on first packet)
/// [1]      0xD7 (215)
/// [2..4]   sequence (unused by receiver)
/// [4..8]   frame_1 = head_frame - 77175   (u32 BE)
/// [8..16]  PTP time of head_frame in raw nanoseconds (u64 BE)
/// [16..20] frame_2 = head_frame           (u32 BE)
/// [20..28] sender PTP clock id            (u64 BE)
/// ```
pub fn build_ptp_anchor_packet(
    first: bool,
    seq: u16,
    head_frame: u32,
    ptp_time_ns: u64,
    clock_id: u64,
) -> [u8; 28] {
    let mut p = [0u8; 28];
    p[0] = if first { 0x90 } else { 0x80 };
    p[1] = 0xD7;
    p[2..4].copy_from_slice(&seq.to_be_bytes());
    p[4..8].copy_from_slice(&head_frame.wrapping_sub(PTP_ANCHOR_LATENCY).to_be_bytes());
    p[8..16].copy_from_slice(&ptp_time_ns.to_be_bytes());
    p[16..20].copy_from_slice(&head_frame.to_be_bytes());
    p[20..28].copy_from_slice(&clock_id.to_be_bytes());
    p
}

/// Fixed-capacity backlog of sent packets for retransmission (PT=0x55 → 0x56).
pub struct PacketBacklog {
    map: HashMap<u16, Vec<u8>>,
    capacity: usize,
}

impl PacketBacklog {
    pub fn new(capacity: usize) -> Self {
        PacketBacklog { map: HashMap::with_capacity(capacity), capacity }
    }

    pub fn insert(&mut self, seq: u16, packet: Vec<u8>) {
        // Evict the entry `capacity` behind us (sequence space is circular).
        self.map.remove(&seq.wrapping_sub(self.capacity as u16));
        self.map.insert(seq, packet);
    }

    pub fn get(&self, seq: u16) -> Option<&Vec<u8>> {
        self.map.get(&seq)
    }
}

/// Shared state the control thread reads to build sync packets.
pub struct SyncState {
    /// Current head timestamp (absolute, sample units).
    pub head_ts: AtomicU64,
    /// Absolute start timestamp.
    pub start_ts: AtomicU64,
    /// Latency in samples.
    pub latency: AtomicU64,
    /// PTP time (ns) at which frame 0 was/will be sent. All anchor packets
    /// extrapolate from this so they are perfectly collinear — re-measuring
    /// the clock per anchor makes the receiver resync (and mute) constantly.
    pub t0_ns: AtomicU64,
    pub sample_rate: u32,
}

impl SyncState {
    /// PTP time corresponding to stream frame `head` on the anchor line.
    pub fn time_of_frame(&self, head: u64) -> u64 {
        let t0 = self.t0_ns.load(Ordering::Relaxed);
        t0 + ((u128::from(head) * 1_000_000_000u128 / u128::from(self.sample_rate)) as u64)
    }
}

/// What the control thread sends every second.
#[derive(Clone, Copy)]
enum SyncMode {
    /// AP2 realtime under PTP: type-215 anchoring announcements.
    PtpAnchor { clock_id: u64 },
    /// AP1/AP2-NTP: 0xD4 sync packets.
    NtpSync,
}

/// Control channel: sends 1 Hz sync packets and answers retransmit requests.
pub struct ControlChannel {
    socket: UdpSocket,
    pub port: u16,
    stop: Arc<AtomicBool>,
    pub backlog: Arc<Mutex<PacketBacklog>>,
}

impl ControlChannel {
    pub fn bind() -> std::io::Result<Self> {
        let socket = UdpSocket::bind(("0.0.0.0", 0))?;
        let port = socket.local_addr()?.port();
        Ok(ControlChannel {
            socket,
            port,
            stop: Arc::new(AtomicBool::new(false)),
            backlog: Arc::new(Mutex::new(PacketBacklog::new(1000))),
        })
    }

    /// Spawn for a PTP session: 1 Hz anchoring announcements (type 215) with
    /// our PTP clock, plus retransmit replies.
    pub fn spawn_ptp(self, dest: SocketAddr, state: Arc<SyncState>, clock_id: u64) -> ControlHandle {
        self.spawn_inner(dest, state, SyncMode::PtpAnchor { clock_id })
    }

    /// Spawn for an NTP session: 1 Hz AP1-style sync packets (0xD4), plus
    /// retransmit replies.
    pub fn spawn(self, dest: SocketAddr, state: Arc<SyncState>) -> ControlHandle {
        self.spawn_inner(dest, state, SyncMode::NtpSync)
    }

    fn spawn_inner(self, dest: SocketAddr, state: Arc<SyncState>, mode: SyncMode) -> ControlHandle {
        let stop = self.stop.clone();
        let backlog = self.backlog.clone();
        let socket = self.socket;
        socket
            .set_read_timeout(Some(std::time::Duration::from_millis(100)))
            .ok();
        let thread = std::thread::spawn(move || {
            let mut first = true;
            let mut sync_seq: u16 = 0;
            let mut last_sync = std::time::Instant::now() - std::time::Duration::from_secs(2);
            let mut buf = [0u8; 128];
            loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                // 1 Hz sync / anchor
                if last_sync.elapsed() >= std::time::Duration::from_secs(1) {
                    match mode {
                        SyncMode::PtpAnchor { clock_id } => {
                            let head64 = state.head_ts.load(Ordering::Relaxed);
                            let head = head64 as u32;
                            let frame_time_ns = state.time_of_frame(head64);
                            let packet =
                                build_ptp_anchor_packet(first, sync_seq, head, frame_time_ns, clock_id);
                            if let Err(e) = socket.send_to(&packet, dest) {
                                warn!("anchor send failed: {e}");
                            } else {
                                debug!(head, "PTP anchor packet sent");
                            }
                        }
                        SyncMode::NtpSync => {
                            let head = state.head_ts.load(Ordering::Relaxed);
                            let start = state.start_ts.load(Ordering::Relaxed);
                            let latency = state.latency.load(Ordering::Relaxed);
                            let rtptime = (head.wrapping_sub(start) + latency) as u32;
                            let ntp = openair_timing::ts_to_ntp(head, state.sample_rate);
                            let packet = build_sync_packet(first, rtptime, latency as u32, ntp);
                            if let Err(e) = socket.send_to(&packet, dest) {
                                warn!("sync send failed: {e}");
                            } else {
                                debug!(rtptime, "sync packet sent");
                            }
                        }
                    }
                    first = false;
                    sync_seq = sync_seq.wrapping_add(1);
                    last_sync = std::time::Instant::now();
                }
                // Retransmit requests
                match socket.recv_from(&mut buf) {
                    Ok((len, peer)) if len >= 8 && buf[1] & 0x7F == 0x55 => {
                        let lost_seq = u16::from_be_bytes([buf[4], buf[5]]);
                        let count = u16::from_be_bytes([buf[6], buf[7]]);
                        let backlog = backlog.lock().unwrap();
                        for i in 0..count {
                            let seq = lost_seq.wrapping_add(i);
                            if let Some(pkt) = backlog.get(seq) {
                                let mut resp = Vec::with_capacity(4 + pkt.len());
                                resp.extend_from_slice(&[0x80, 0xD6]);
                                resp.extend_from_slice(&seq.to_be_bytes());
                                resp.extend_from_slice(pkt);
                                let _ = socket.send_to(&resp, peer);
                            } else {
                                debug!(seq, "retransmit miss (not in backlog)");
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) => {
                        warn!("control socket error: {e}");
                        break;
                    }
                }
            }
        });
        ControlHandle { stop: self.stop, thread: Some(thread) }
    }
}

/// Handle to stop the control thread.
pub struct ControlHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for ControlHandle {
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
    fn audio_packet_layout() {
        let mut cipher = AudioCipher::new(&[7u8; 32]);
        let payload = vec![0xABu8; 100];
        let pkt = build_audio_packet(&mut cipher, true, 0x1234, 0xDEADBEEF, 0xCAFEBABE, &payload);
        assert_eq!(pkt.len(), 12 + 100 + 16 + 8);
        assert_eq!(pkt[0], 0x80);
        assert_eq!(pkt[1], 0xE0); // marker set on first packet
        assert_eq!(&pkt[2..4], &0x1234u16.to_be_bytes());
        assert_eq!(&pkt[4..8], &0xDEADBEEFu32.to_be_bytes());
        assert_eq!(&pkt[8..12], &0xCAFEBABEu32.to_be_bytes());
        // First nonce is counter 0
        assert_eq!(&pkt[pkt.len() - 8..], &0u64.to_le_bytes());

        let pkt2 = build_audio_packet(&mut cipher, false, 0x1235, 0, 0, &payload);
        assert_eq!(pkt2[1], 0x60); // no marker
        assert_eq!(&pkt2[pkt2.len() - 8..], &1u64.to_le_bytes());
    }

    #[test]
    fn audio_packet_decrypts_with_wire_nonce_and_aad() {
        let shk = [9u8; 32];
        let mut cipher = AudioCipher::new(&shk);
        let payload = b"hello airplay".to_vec();
        let pkt = build_audio_packet(&mut cipher, false, 1, 42, 99, &payload);

        // Receiver side: reconstruct nonce from packet tail, AAD from header.
        let cipher_rx = ChaCha20Poly1305::new(Key::from_slice(&shk));
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(&pkt[pkt.len() - 8..]);
        let ct = &pkt[12..pkt.len() - 8];
        let plain = cipher_rx
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: ct, aad: &pkt[4..12] })
            .unwrap();
        assert_eq!(plain, payload);
    }

    #[test]
    fn buffered_audio_block_layout_and_decrypts() {
        let shk = [5u8; 32];
        let mut cipher = AudioCipher::new(&shk);
        let payload = b"raw aac-lc frame bytes".to_vec();
        let seq: u32 = 0x00ABCDEF; // within 23-bit range
        let rtptime: u32 = 1024;
        let block = build_buffered_audio_block(&mut cipher, seq, rtptime, AAC_44100_F24_2_SSRC, &payload);

        // Length prefix includes the 2 length bytes themselves.
        let declared_len = u16::from_be_bytes([block[0], block[1]]) as usize;
        assert_eq!(declared_len, block.len());

        // word0 = 0x80800000 | (seq & 0x7FFFFF)
        let word0 = u32::from_be_bytes([block[2], block[3], block[4], block[5]]);
        assert_eq!(word0, 0x8080_0000 | (seq & 0x007F_FFFF));

        let rtptime_be = u32::from_be_bytes([block[6], block[7], block[8], block[9]]);
        assert_eq!(rtptime_be, rtptime);
        let ssrc_be = u32::from_be_bytes([block[10], block[11], block[12], block[13]]);
        assert_eq!(ssrc_be, AAC_44100_F24_2_SSRC);

        // Receiver-style decrypt: nonce from tail, AAD = block[4..12]
        // (offset by 2 for the length prefix that precedes the RTP-ish header).
        let header_start = 2;
        let aad = &block[header_start + 4..header_start + 12];
        let ct = &block[header_start + 12..block.len() - 8];
        let nonce_bytes = &block[block.len() - 8..];

        let cipher_rx = ChaCha20Poly1305::new(Key::from_slice(&shk));
        let mut nonce = [0u8; 12];
        nonce[4..].copy_from_slice(nonce_bytes);
        let plain = cipher_rx
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: ct, aad })
            .unwrap();
        assert_eq!(plain, payload);
    }

    #[test]
    fn sync_packet_layout() {
        let ntp = (0xAABB_CCDDu64 << 32) | 0x1122_3344;
        let p = build_sync_packet(true, 100_000, 66_150, ntp);
        assert_eq!(p[0], 0x90);
        assert_eq!(p[1], 0xD4);
        assert_eq!(&p[2..4], &7u16.to_be_bytes());
        assert_eq!(&p[4..8], &(100_000u32 - 66_150).to_be_bytes());
        assert_eq!(&p[8..12], &0xAABB_CCDDu32.to_be_bytes());
        assert_eq!(&p[12..16], &0x1122_3344u32.to_be_bytes());
        assert_eq!(&p[16..20], &100_000u32.to_be_bytes());
    }

    #[test]
    fn backlog_evicts_old_packets() {
        let mut b = PacketBacklog::new(10);
        for seq in 0u16..25 {
            b.insert(seq, vec![seq as u8]);
        }
        assert!(b.get(5).is_none());
        assert!(b.get(24).is_some());
        assert!(b.get(15).is_some());
    }
}
