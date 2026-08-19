//! High-level streaming API. Step 4 (with PTP pulled forward from Step 6):
//! single-device realtime ALAC streaming.
//!
//! Pipeline: pair → SETUP(timing=PTP) → SETUP(stream) → RECORD →
//! SETRATEANCHORTIME(rate=1) → paced RTP audio + PTP master + /feedback →
//! TEARDOWN.
use std::io::Write;
use std::net::{SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use openair_audio_codec::{alac_encode_verbatim, AacEncoder, AAC_FRAMES_PER_PACKET, FRAMES_PER_PACKET};
use openair_audio_rtp::{
    build_audio_packet, build_buffered_audio_block, AudioCipher, ControlChannel, SyncState,
    AAC_44100_F24_2_SSRC,
};
use openair_core::metadata::NowPlaying;
use openair_rtsp::{StreamFormat, StreamSession, TimingConfig};
use openair_timing::{ptp_now_ns, ptp_ns_to_secs_frac, PtpMaster};
use tracing::{debug, info, trace, warn};

mod pairings;
mod source;
mod stats;
pub use pairings::PairingStore;
pub use source::{CaptureSource, SineSource, WavSource};
pub use stats::{
    ReceiverStat, ReceiverState, StreamCommand, StreamStats, TRIM_MAX_DB, TRIM_MIN_DB,
};

pub(crate) const SAMPLE_RATE: u32 = 44100;

/// Open a paired, encrypted RTSP session with the right pairing flavor:
/// stored HomeKit credentials (Apple TV / HomePod → pair-verify) if we have
/// them for this device-id, Transient pairing (Shairport, AirPort Express)
/// otherwise.
fn connect_session(
    addr: SocketAddr,
    device_id: &str,
) -> Result<StreamSession, Box<dyn std::error::Error>> {
    if let Ok(store) = PairingStore::load() {
        if let Some(peer) = store.peer(device_id) {
            let identity = store.identity()?;
            info!(device_id, "using stored HomeKit pairing (pair-verify)");
            let conn = openair_rtsp::pair_verify(addr, device_id, identity, peer)?;
            return Ok(StreamSession::from_connection(conn)?);
        }
    }
    Ok(StreamSession::connect(addr, device_id)?)
}

/// Connect the reverse "event" TCP channel (port from SETUP phase 1).
///
/// Apple receivers (Apple TV / HomePod) expect the sender to connect here
/// before RECORD completes — owntone does the same ("reverse connection,
/// used to receive playback events"). Without it RECORD stalls until our
/// read timeout. We never send anything; a drain thread discards whatever
/// the receiver pushes. Shairport doesn't need this — warn-and-continue.
fn open_event_channel(
    peer_ip: std::net::IpAddr,
    event_port: u16,
    event_keys: Option<([u8; 32], [u8; 32])>,
) -> Option<TcpStream> {
    match openair_core::net::connect_from_best_source(SocketAddr::new(peer_ip, event_port)) {
        Ok(s) => {
            s.set_nodelay(true).ok();
            if let (Ok(rdr), Ok(wtr)) = (s.try_clone(), s.try_clone()) {
                std::thread::spawn(move || event_reader(rdr, wtr, event_keys));
            }
            info!(event_port, "event channel connected");
            Some(s)
        }
        Err(e) => {
            warn!("event channel connect failed (continuing): {e}");
            None
        }
    }
}

/// End of the header block (index just past the blank line), if present.
fn header_block_end(msg: &[u8]) -> Option<usize> {
    msg.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Value of a header, case-insensitively, from a header block.
fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    let want = name.to_ascii_lowercase();
    headers.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim().to_ascii_lowercase() == want).then(|| v.trim())
    })
}

/// Total length of the RTSP message at the front of `msg`, once fully arrived.
///
/// Returns `None` while the message is still incomplete — event messages span
/// several encrypted frames (the observed `POST /command` is 2519 bytes across
/// three), so a reply must wait for the whole thing.
fn rtsp_message_len(msg: &[u8]) -> Option<usize> {
    let head = header_block_end(msg)?;
    let headers = String::from_utf8_lossy(&msg[..head]);
    let body_len = header_value(&headers, "Content-Length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    (msg.len() >= head + body_len).then_some(head + body_len)
}

/// Build the RTSP response to an event-channel request.
///
/// The receiver only needs acknowledgement; it carries no body. `CSeq` must be
/// echoed back or the request is treated as unanswered.
fn event_response(request: &[u8]) -> Vec<u8> {
    let head = header_block_end(request).unwrap_or(request.len());
    let headers = String::from_utf8_lossy(&request[..head]);
    let cseq = header_value(&headers, "CSeq").unwrap_or("0");
    format!(
        "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nServer: AirTunes/770.8.1\r\nContent-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// Read the reverse event channel and answer what the receiver asks.
///
/// The receiver pushes RTSP requests here (observed: `POST /command` carrying an
/// `updateInfo` binary plist), framed exactly like the control channel
/// (`uint16_le(len) || ciphertext || 16-byte tag`) but keyed under the
/// `Events-Salt` labels. **Answering is not optional**: an Apple TV that gets no
/// response tears the whole session down after ~30 s, taking the audio with it.
///
/// Key direction is hardware-verified (2026-08-17, AppleTV6,2 / AirTunes
/// 960.13.1): the accessory encrypts with `Events-Write-Encryption-Key`, i.e.
/// the labels are from *its* perspective, the reverse of the control channel.
/// So we read with `events_write` and reply with `events_read`.
fn event_reader(mut rdr: TcpStream, mut wtr: TcpStream, event_keys: Option<([u8; 32], [u8; 32])>) {
    use openair_crypto::ChaChaChannel;

    let Some((events_write, events_read)) = event_keys else {
        warn!("event channel has no keys — cannot answer the receiver");
        return;
    };
    let mut rx = ChaChaChannel::new(&events_write);
    let mut tx = ChaChaChannel::new(&events_read);

    let mut frames: Vec<u8> = Vec::new(); // undecrypted bytes
    let mut msg: Vec<u8> = Vec::new(); // decrypted, reassembled
    let mut buf = [0u8; 4096];

    loop {
        match std::io::Read::read(&mut rdr, &mut buf) {
            Ok(0) => {
                warn!("event channel closed by receiver");
                break;
            }
            Ok(n) => {
                frames.extend_from_slice(&buf[..n]);
                // A frame can span reads; consume only whole ones.
                while frames.len() >= 2 {
                    let len = u16::from_le_bytes([frames[0], frames[1]]) as usize;
                    let frame_len = 2 + len + 16;
                    if frames.len() < frame_len {
                        break;
                    }
                    let frame: Vec<u8> = frames.drain(..frame_len).collect();
                    match rx.decrypt(&frame) {
                        Ok(plain) => msg.extend_from_slice(&plain),
                        Err(e) => {
                            warn!("event frame failed to decrypt: {e}");
                            return; // counter is desynced; nothing sane follows
                        }
                    }
                }
                // A message can span frames; answer only complete ones.
                while let Some(end) = rtsp_message_len(&msg) {
                    let request: Vec<u8> = msg.drain(..end).collect();
                    let first_line = String::from_utf8_lossy(&request)
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_string();
                    // At `--debug 2`, dump everything the receiver said. The
                    // body is a binary plist, so log printable text and hex
                    // side by side rather than guessing which is readable.
                    trace!(
                        bytes = request.len(),
                        text = %String::from_utf8_lossy(&request),
                        hex = %request.iter().map(|b| format!("{b:02x}")).collect::<String>(),
                        "event message (full)"
                    );
                    let response = event_response(&request);
                    match tx.encrypt(&response) {
                        Ok(framed) => match std::io::Write::write_all(&mut wtr, &framed) {
                            Ok(()) => info!(request = %first_line, "event channel: answered 200 OK"),
                            Err(e) => {
                                warn!(request = %first_line, "event reply write failed: {e}");
                                return;
                            }
                        },
                        Err(e) => warn!("event reply encrypt failed: {e}"),
                    }
                }
            }
            Err(e) => {
                warn!("event channel read error: {e}");
                break;
            }
        }
    }
}

/// One-time Normal HomeKit pair-setup with PIN (Apple TV / HomePod).
///
/// Shows a PIN on the device; `pin_provider` must return it (e.g. from
/// stdin). On success the credentials are persisted, and every later
/// connection to this device-id automatically uses pair-verify.
pub fn pair_device(
    addr: SocketAddr,
    device_id: &str,
    pin_provider: &mut dyn FnMut() -> String,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut store = PairingStore::load()?;
    // Persist the identity before pairing so a crash after M6 can't strand
    // an accessory that stored our LTPK we no longer have.
    store.ensure_saved()?;
    let identity = store.identity()?;
    let peer = openair_rtsp::pair_setup_normal(addr, device_id, &identity, pin_provider)?;
    store.set_peer(device_id, &peer)?;
    info!(device_id, "pairing stored — future connections will use pair-verify");
    Ok(())
}

/// A source of interleaved-stereo, 44100 Hz i16 audio frames.
///
/// Implementors are pulled from the pacing loop in [`stream_audio`]; `fill`
/// should be non-blocking (or block for at most a few packet durations) so
/// the RTP pacing stays accurate.
pub trait AudioSource {
    /// Fills `buf` (interleaved stereo i16, 44100 Hz) with up to
    /// `buf.len()/2` frames. Returns the number of FRAMES written; 0 means
    /// end of stream.
    fn fill(&mut self, buf: &mut [i16]) -> usize;

    /// True for continuous live sources (system capture) where a sustained
    /// stretch of silence means "playback paused" and the buffered pipeline
    /// should pause/auto-resume the AirPlay stream. False for finite sources
    /// (WAV, tone) where a quiet passage is just quiet music, not a pause.
    fn is_live(&self) -> bool {
        false
    }
}

/// Stream audio pulled from `source` to `addr`. This is the shared pipeline
/// behind [`stream_tone`] and any other `AudioSource` producer (e.g. WAV
/// file playback): pair → SETUP(timing=PTP) → SETUP(stream) → RECORD →
/// SETRATEANCHORTIME(rate=1) → paced RTP audio + PTP master + /feedback →
/// TEARDOWN.
pub fn stream_audio(
    addr: SocketAddr,
    device_id: &str,
    source: &mut dyn AudioSource,
    volume_db: Option<f32>,
) -> Result<(), Box<dyn std::error::Error>> {
    // --- Control channel (retransmit replies; no AP1-style sync under PTP) ---
    let control = ControlChannel::bind()?;
    let control_port = control.port;

    // --- RTSP negotiation ---
    let mut session = connect_session(addr, device_id)?;
    let peer_ip = session.peer_ip();

    // PTP master must be running before the receiver starts monitoring us.
    let ptp = PtpMaster::start(peer_ip)?;

    session.setup_timing(TimingConfig::Ptp)?;
    session.setup_stream(StreamFormat::AlacRealtime, control_port)?;
    let ports = session.ports;

    // Reverse event channel — must be connected before RECORD on Apple
    // receivers (held open for the whole session).
    let _event = open_event_channel(peer_ip, ports.event_port, session.event_keys());

    // Real Apple receivers need SETPEERS to know which clock to monitor;
    // Shairport ignores it (warn-and-continue keeps older receivers happy).
    if let Err(e) = session.set_peers() {
        warn!("SETPEERS failed (continuing): {e}");
    }

    // Let the receiver's clock daemon converge on our PTP clock before audio
    // starts: nqptp resets its clock records at SETUP and its offset
    // smoothing needs ~1-2s of follow_ups; starting audio immediately causes
    // audible resync churn in the first seconds.
    std::thread::sleep(Duration::from_millis(1500));

    // Which timeline do anchors live on? Ours (Shairport slaves to us), or
    // the receiver's own grandmaster (Apple TV/HomePod — we yielded toward
    // it during the warm-up above and measured our offset to its clock).
    let tl = ptp.timeline_for(peer_ip);
    info!(
        gm = format!("{:016x}", tl.gm_id),
        offset_ms = tl.offset_ns as f64 / 1e6,
        foreign = tl.gm_id != ptp.clock_id,
        "anchor timeline"
    );

    // Shared clock state for the control thread. t0 = PTP time of frame 0;
    // all anchor packets extrapolate from it (collinear anchor line).
    let t0_ns = ptp_now_ns();
    let state = Arc::new(SyncState {
        head_ts: std::sync::atomic::AtomicU64::new(0),
        start_ts: std::sync::atomic::AtomicU64::new(0),
        latency: std::sync::atomic::AtomicU64::new(0),
        t0_ns: std::sync::atomic::AtomicU64::new(t0_ns),
        timeline_gm: std::sync::atomic::AtomicU64::new(tl.gm_id),
        timeline_offset_ns: std::sync::atomic::AtomicI64::new(tl.offset_ns),
        sample_rate: SAMPLE_RATE,
    });
    let backlog = control.backlog.clone();
    let _control_handle = control.spawn_ptp(
        SocketAddr::new(peer_ip, ports.control_port),
        state.clone(),
        ptp.clock_id,
    );

    // --- RECORD + play rate ---
    let mut seq: u16 = rand_seq();
    let first_rtptime: u32 = 0;
    session.record(seq, first_rtptime)?;

    // rate=1 flips ap2_play_enabled on the receiver. Real Apple receivers
    // 400 the rate-only variant (hardware-verified on AppleTV5,3) — they
    // need the full anchor plist. We send the same anchor line the
    // control-channel type-215 packets announce (frame 0 at t0, translated
    // onto the active timeline), so the anchor sources stay collinear.
    let anchor_ns = t0_ns.wrapping_add_signed(tl.offset_ns);
    let (t0_secs, t0_frac) = ptp_ns_to_secs_frac(anchor_ns);
    session.set_rate_anchor(tl.gm_id, first_rtptime, t0_secs, t0_frac, 1)?;

    if let Some(db) = volume_db {
        if let Err(e) = session.set_volume(db) {
            warn!("set_volume failed (continuing): {e}");
        }
    }

    // --- Audio send loop ---
    let audio_sock = UdpSocket::bind(("0.0.0.0", 0))?;
    audio_sock.connect(SocketAddr::new(peer_ip, ports.data_port))?;
    let mut cipher = AudioCipher::new(&session.shk);

    let packet_dur = Duration::from_secs_f64(FRAMES_PER_PACKET as f64 / SAMPLE_RATE as f64);
    let start_instant = Instant::now();
    let mut last_feedback = Instant::now();

    info!(data_port = ports.data_port, "streaming audio");

    let mut n: u32 = 0;
    loop {
        let mut samples = [0i16; FRAMES_PER_PACKET * 2];
        let frames = source.fill(&mut samples);
        if frames == 0 {
            break;
        }
        if frames < FRAMES_PER_PACKET {
            // Zero-pad the final partial packet.
            for v in &mut samples[frames * 2..] {
                *v = 0;
            }
        }
        let payload = alac_encode_verbatim(&samples);

        let rtptime = first_rtptime.wrapping_add(n * FRAMES_PER_PACKET as u32);
        let packet =
            build_audio_packet(&mut cipher, n == 0, seq, rtptime, session.session_id, &payload);
        audio_sock.send(&packet)?;
        backlog.lock().unwrap().insert(seq, packet);
        seq = seq.wrapping_add(1);
        // Keep the control thread's view of the stream head current.
        state.head_ts.store(
            u64::from(rtptime) + FRAMES_PER_PACKET as u64,
            Ordering::Relaxed,
        );

        if last_feedback.elapsed() >= Duration::from_secs(2) {
            if let Err(e) = session.feedback() {
                warn!("feedback failed: {e}");
            }
            last_feedback = Instant::now();
        }

        // Pace to real time: packet n+1 is due at start + (n+1)*packet_dur
        let due = start_instant + packet_dur * (n + 1);
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }

        n += 1;
    }

    info!("stream finished, tearing down");
    session.set_rate(0).ok();
    session.teardown()?;
    Ok(())
}

/// Lead window (in samples) the buffered send loop tries to keep queued
/// ahead of wall-clock playback: while `frames_sent - elapsed_frames` is at
/// or above this, we sleep briefly instead of encoding/sending more.
const BUFFERED_LEAD_SAMPLES: i64 = 88_200; // 2s @ 44100 Hz
/// Default PTP lead time before the anchor's rtpTime=0 is scheduled to play.
/// This IS the end-to-end latency of a buffered stream (plus capture-side
/// buffering) — the realtime pipeline's ~2 s is fixed by protocol constants,
/// but the buffered anchor is the sender's choice. 500 ms matches Apple's
/// typical buffered latency and is comfortable on a LAN.
const BUFFERED_ANCHOR_LEAD_MS_DEFAULT: u64 = 500;

/// Peak |sample| below which a packet counts as silence, for live-capture
/// pause detection (~ -54 dBFS). Real system playback sits far above this;
/// a paused source is exact zeros (WASAPI loopback stops delivering, so the
/// capture source pads zeros).
const SILENCE_PEAK: u16 = 64;
/// How long a live source must stay silent before the AirPlay stream is
/// paused (`rate=0`). Auto-resumes (re-anchor) the instant audio returns.
const PAUSE_AFTER_SILENCE: Duration = Duration::from_millis(1500);

/// How many times a dropped receiver is re-established (re-pair → SETUP →
/// RECORD → re-anchor) before it is given up on. Only live (capture)
/// streams reconnect — a finite tone/file just loses the receiver.
const MAX_RECONNECT_ATTEMPTS: u32 = 3;
/// Base backoff between reconnect attempts; attempt N waits N × this so a
/// receiver that's briefly off (TV asleep, Wi-Fi blip) is retried soon while
/// a truly gone one isn't hammered.
const RECONNECT_BACKOFF: Duration = Duration::from_secs(2);

/// Auto-latency: if the newest queued frame's play-deadline stays within this
/// of "now" across a whole evaluation window, the buffer is dangerously
/// shallow (the network/receiver can't keep up) and the latency is stepped up.
/// For a live capture the receiver's jitter buffer is ≈ the anchor latency, so
/// a deeper anchor = more headroom.
const UNDERRUN_LEAD_FLOOR: Duration = Duration::from_millis(120);
/// How much to raise the anchor latency each time underrun risk is detected.
const AUTO_LATENCY_STEP_MS: u64 = 250;
/// Ceiling for auto-raised latency (a bump-only heuristic never lowers it).
const AUTO_LATENCY_MAX_MS: u64 = 2000;
/// Evaluation window: the minimum lead seen over this span is what's compared
/// against the floor, so a single transient dip can't ratchet latency up.
const AUTO_LATENCY_WINDOW: Duration = Duration::from_millis(1000);
/// Wait this long after a bump before considering another, so the deeper
/// buffer has time to fill and stabilise before we judge it again.
const AUTO_LATENCY_COOLDOWN: Duration = Duration::from_secs(5);

/// How often the current track is re-stated to receivers. The first send goes
/// out before any audio has, and a receiver may ignore metadata for a stream it
/// has not begun rendering; re-sending is ~90 bytes and keeps a late-joining or
/// slow-to-start receiver's screen correct.
const METADATA_RESEND_INTERVAL: Duration = Duration::from_secs(10);

/// Stream audio pulled from `source` to `addr` using AirPlay 2's BUFFERED
/// pipeline (stream type 103, AAC-LC): pair → SETUP(timing=PTP) →
/// SETUP(stream type=103) → TCP connect to dataPort → RECORD →
/// SETRATEANCHORTIME(full anchor) → send-ahead-paced AAC blocks over TCP +
/// PTP master + /feedback → TEARDOWN.
///
/// Unlike [`stream_audio`] (realtime ALAC over UDP, paced to real time),
/// this pipeline sends over a TCP connection to `dataPort` and paces with a
/// send-ahead window: it keeps encoding/sending as fast as the source and
/// encoder allow, only sleeping once it's ~2s ahead of wall-clock playback.
/// The anchor is set once via RTSP (not the control-channel type-215 packets
/// realtime uses), so `ControlChannel`'s PTP anchor loop is not spawned here
/// — the control port is bound but left idle (SETUP still requires one).
pub fn stream_audio_buffered(
    addr: SocketAddr,
    device_id: &str,
    source: &mut dyn AudioSource,
    volume_db: Option<f32>,
) -> Result<(), Box<dyn std::error::Error>> {
    stream_audio_buffered_with_latency(
        addr,
        device_id,
        source,
        volume_db,
        BUFFERED_ANCHOR_LEAD_MS_DEFAULT,
    )
}

/// [`stream_audio_buffered`] with an explicit anchor lead (end-to-end
/// latency) in milliseconds. Values below ~300 ms risk underruns while the
/// receiver's clock estimate is still converging.
pub fn stream_audio_buffered_with_latency(
    addr: SocketAddr,
    device_id: &str,
    source: &mut dyn AudioSource,
    volume_db: Option<f32>,
    latency_ms: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    stream_audio_buffered_multi(
        &[GroupTarget {
            addr,
            device_id: device_id.to_string(),
            offset_ms: 0,
        }],
        source,
        volume_db,
        latency_ms,
        None,
        None,
        None,
    )
}

/// One receiver in a buffered (possibly multi-room) stream.
pub struct GroupTarget {
    pub addr: SocketAddr,
    pub device_id: String,
    /// Extra play delay for this receiver in milliseconds (+ = later,
    /// − = earlier), added to its anchor. Compensates downstream amp/DSP
    /// latency so rooms line up audibly.
    pub offset_ms: i64,
}

/// One receiver's live state inside a (possibly multi-room) buffered stream.
struct BufferedReceiver {
    name: String,
    /// Where to reconnect if this receiver drops (live streams only).
    addr: SocketAddr,
    device_id: String,
    session: StreamSession,
    cipher: AudioCipher,
    /// Per-receiver anchor offset in ns (from `GroupTarget::offset_ms`).
    offset_ns: i64,
    /// Per-receiver volume trim in dB, applied on top of the group's master
    /// level. A *trim* rather than an absolute level because `--handoff`
    /// mirrors the Windows master onto every receiver — an absolute
    /// per-receiver volume would be flattened the moment the user touched the
    /// Windows slider, whereas a trim preserves the balance they dialled in.
    trim_db: f32,
    /// Bounded queue to this receiver's TCP writer thread. `None` once closed.
    tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    writer: Option<std::thread::JoinHandle<()>>,
    /// Keeps the reverse event channel open for the session lifetime.
    _event: Option<TcpStream>,
    /// Keeps the (idle) control socket bound for the session lifetime.
    _control: ControlChannel,
    alive: bool,
}

/// A receiver re-established on a background thread after it dropped: paired,
/// SETUP, event channel + SETPEERS done, and the TCP data connection open —
/// but NOT yet RECORD'd or anchored (the main loop does those with the live
/// anchor baseline so the rejoining receiver lands in sync with the group).
struct PreparedReceiver {
    name: String,
    addr: SocketAddr,
    device_id: String,
    offset_ns: i64,
    /// Carried through a reconnect so a receiver keeps the trim the user set
    /// before it dropped, rather than silently snapping back to the group
    /// level when it rejoins.
    trim_db: f32,
    session: StreamSession,
    cipher: AudioCipher,
    control: ControlChannel,
    event: Option<TcpStream>,
    data_stream: TcpStream,
}

/// An in-flight reconnect: the background thread sends `Ok(prepared)` on the
/// first successful attempt or `Err(())` once it gives up.
struct ReconnectHandle {
    name: String,
    trim_db: f32,
    /// Kept so an observer can list a recovering receiver without waiting for
    /// it to rejoin — otherwise a dropped receiver simply disappears from the
    /// dashboard until it comes back, which reads like data loss.
    addr: SocketAddr,
    offset_ns: i64,
    rx: std::sync::mpsc::Receiver<Result<PreparedReceiver, ()>>,
}

/// Do the slow part of establishing a receiver (pair → SETUP → event →
/// SETPEERS → TCP connect to dataPort). Shared by fresh setup and reconnect.
/// Returns everything the caller needs to RECORD + anchor + start the writer.
fn prepare_receiver(
    target_addr: SocketAddr,
    device_id: &str,
    offset_ns: i64,
    trim_db: f32,
) -> Result<PreparedReceiver, Box<dyn std::error::Error>> {
    let name = format!("{target_addr}");
    let control = ControlChannel::bind()?;
    let mut session = connect_session(target_addr, device_id)?;
    let peer_ip = session.peer_ip();
    session.setup_timing(TimingConfig::Ptp)?;
    session.setup_stream(StreamFormat::AacBuffered, control.port)?;
    let event = open_event_channel(peer_ip, session.ports.event_port, session.event_keys());
    if let Err(e) = session.set_peers() {
        warn!("SETPEERS failed (continuing): {e}");
    }
    let data_stream =
        openair_core::net::connect_from_best_source(SocketAddr::new(peer_ip, session.ports.data_port))?;
    data_stream.set_nodelay(true).ok();
    let cipher = AudioCipher::new(&session.shk);
    Ok(PreparedReceiver {
        name,
        addr: target_addr,
        device_id: device_id.to_string(),
        offset_ns,
        trim_db,
        session,
        cipher,
        control,
        event,
        data_stream,
    })
}

/// Spawn a background thread that retries [`prepare_receiver`] up to
/// [`MAX_RECONNECT_ATTEMPTS`] times with increasing backoff, reporting the
/// first success (or final failure) back to the main loop. Runs off the audio
/// thread so healthy receivers keep playing uninterrupted during the
/// seconds-long re-pair/SETUP.
fn spawn_reconnect(
    addr: SocketAddr,
    device_id: String,
    offset_ns: i64,
    trim_db: f32,
    delay_first: bool,
) -> ReconnectHandle {
    let name = format!("{addr}");
    let (tx, rx) = std::sync::mpsc::channel();
    let thread_name = name.clone();
    std::thread::spawn(move || {
        for attempt in 1..=MAX_RECONNECT_ATTEMPTS {
            if delay_first || attempt > 1 {
                std::thread::sleep(RECONNECT_BACKOFF * attempt);
            }
            info!(receiver = %thread_name, attempt, "reconnect attempt");
            match prepare_receiver(addr, &device_id, offset_ns, trim_db) {
                Ok(prep) => {
                    let _ = tx.send(Ok(prep));
                    return;
                }
                Err(e) => warn!(receiver = %thread_name, attempt, "reconnect failed: {e}"),
            }
        }
        warn!(receiver = %thread_name, "giving up reconnecting");
        let _ = tx.send(Err(()));
    });
    ReconnectHandle {
        name,
        trim_db,
        addr,
        offset_ns,
        rx,
    }
}

/// Spawn the per-receiver TCP writer thread: it drains its bounded queue and
/// writes each encrypted block to `stream`, exiting (which the main loop sees
/// as a drop) on the first write error.
fn spawn_writer(
    mut stream: TcpStream,
    name: String,
) -> (std::sync::mpsc::SyncSender<Vec<u8>>, std::thread::JoinHandle<()>) {
    // ~256 blocks ≈ 6 s of audio: enough to absorb TCP hiccups, small enough
    // to bound memory and detect a truly dead peer.
    let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(256);
    let handle = std::thread::spawn(move || {
        for block in rx {
            if let Err(e) = stream.write_all(&block) {
                warn!(receiver = %name, "data write failed: {e}");
                break; // dropping rx signals the main loop
            }
        }
    });
    (tx, handle)
}

/// Turn a background-prepared receiver into a live group member: RECORD at the
/// current stream head, anchor it onto the group's current anchor line (so it
/// lands in sync with whoever's still playing), match volume/pause state, and
/// start its writer. Returns `None` if RECORD or the anchor fails.
#[allow(clippy::too_many_arguments)]
fn finish_reconnect(
    ptp: &PtpMaster,
    prep: PreparedReceiver,
    seq: u32,
    rtptime: u32,
    anchor_t_local: u64,
    anchor_rtptime: u32,
    volume_db: Option<f32>,
    paused: bool,
) -> Option<BufferedReceiver> {
    let mut br = BufferedReceiver {
        name: prep.name,
        addr: prep.addr,
        device_id: prep.device_id,
        session: prep.session,
        cipher: prep.cipher,
        offset_ns: prep.offset_ns,
        trim_db: prep.trim_db,
        tx: None,
        writer: None,
        _event: prep.event,
        _control: prep.control,
        alive: true,
    };
    if let Err(e) = br.session.record(seq as u16, rtptime) {
        warn!(receiver = %br.name, "rejoin RECORD failed: {e}");
        return None;
    }
    // Express the group's anchor line at the CURRENT position rather than at
    // its origin. Both describe the same line, but anchoring at
    // (anchor_rtptime, anchor_t_local) means telling a receiver that joins 30 s
    // in "position 0 plays 30 s ago" — a reference instant in the past, which
    // receivers can reject or mishandle. Anchoring at (rtptime, when rtptime is
    // due) is the same schedule stated forward.
    let play_at = play_deadline_ns(anchor_t_local, anchor_rtptime, rtptime);
    if let Err(e) = anchor_receiver(ptp, &mut br, play_at, rtptime) {
        warn!(receiver = %br.name, "rejoin anchor failed: {e}");
        return None;
    }
    apply_volume(&mut br, volume_db);
    if paused {
        // Group is mid-pause; keep the newcomer quiet until the group resumes.
        br.session.set_rate(0).ok();
    }
    let (tx, handle) = spawn_writer(prep.data_stream, br.name.clone());
    br.tx = Some(tx);
    br.writer = Some(handle);
    info!(receiver = %br.name, "rejoined group");
    Some(br)
}

/// Remove dropped receivers from `group`; for live streams schedule a
/// background reconnect for each so a receiver that briefly disappears (TV
/// asleep, Wi-Fi blip) rejoins automatically.
fn reap_dead(group: &mut Vec<BufferedReceiver>, handles: &mut Vec<ReconnectHandle>, reconnect: bool) {
    let mut i = 0;
    while i < group.len() {
        if group[i].alive {
            i += 1;
            continue;
        }
        let mut dead = group.remove(i);
        // Best effort: tell the receiver this session is finished. Only the
        // DATA socket dies on a drop — the RTSP control connection is still
        // healthy (/feedback keeps succeeding) — so this usually gets through.
        // Without it every drop orphans a session on the receiver, which is the
        // leading explanation for an Apple TV that accepts metadata (200 OK) but
        // stops displaying it after the first reconnect.
        match dead.session.teardown() {
            Ok(()) => debug!(receiver = %dead.name, "tore down dropped session"),
            Err(e) => debug!(receiver = %dead.name, "teardown of dropped session failed: {e}"),
        }
        if reconnect {
            info!(receiver = %dead.name, "receiver dropped — scheduling reconnect");
            handles.push(spawn_reconnect(
                dead.addr,
                dead.device_id.clone(),
                dead.offset_ns,
                dead.trim_db,
                true,
            ));
        }
    }
}

impl BufferedReceiver {
    /// Queue an encrypted block, waiting up to ~1 s if the receiver's TCP
    /// window is momentarily stalled. A receiver that stays stalled (or
    /// whose connection died) is dropped from the group — the others keep
    /// playing.
    fn queue(&mut self, block: Vec<u8>) {
        use std::sync::mpsc::TrySendError;
        let Some(tx) = self.tx.as_ref() else {
            self.alive = false;
            return;
        };
        let mut block = block;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match tx.try_send(block) {
                Ok(()) => return,
                Err(TrySendError::Full(b)) => {
                    if Instant::now() >= deadline {
                        warn!(receiver = %self.name, "receiver stalled — dropping from group");
                        self.alive = false;
                        return;
                    }
                    block = b;
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(TrySendError::Disconnected(_)) => {
                    warn!(receiver = %self.name, "receiver connection lost — dropping from group");
                    self.alive = false;
                    return;
                }
            }
        }
    }

    fn finish(&mut self) {
        // Closing the channel lets the writer drain its queue and exit.
        drop(self.tx.take());
        if let Some(w) = self.writer.take() {
            let _ = w.join();
        }
        if self.alive {
            self.session.set_rate(0).ok();
            if let Err(e) = self.session.teardown() {
                warn!(receiver = %self.name, "teardown failed: {e}");
            }
        }
    }
}

/// Compute and push one receiver's SETRATEANCHORTIME so that stream position
/// `rtptime` is heard at the shared instant `t_local_ns` (on OUR PTP clock),
/// translated onto the clock that receiver actually follows and shifted by
/// its user offset. Used for the initial anchor and for every resume.
fn anchor_receiver(
    ptp: &PtpMaster,
    r: &mut BufferedReceiver,
    t_local_ns: u64,
    rtptime: u32,
) -> Result<(), openair_rtsp::SessionError> {
    let tl = ptp.timeline_for(r.session.peer_ip());
    let play_ns = t_local_ns
        .wrapping_add_signed(r.offset_ns)
        .wrapping_add_signed(tl.offset_ns);
    let (secs, frac) = ptp_ns_to_secs_frac(play_ns);
    info!(
        receiver = %r.name,
        gm = format!("{:016x}", tl.gm_id),
        clock_offset_ms = tl.offset_ns as f64 / 1e6,
        user_offset_ms = r.offset_ns as f64 / 1e6,
        foreign = tl.gm_id != ptp.clock_id,
        "anchor"
    );
    r.session.set_rate_anchor(tl.gm_id, rtptime, secs, frac, 1)
}

/// Our-clock instant (ns) at which stream position `rtptime` is scheduled to
/// play, per the current group anchor line `(anchor_rtptime → anchor_t_local)`.
/// Used by auto-latency to measure how much buffer headroom is left.
fn play_deadline_ns(anchor_t_local: u64, anchor_rtptime: u32, rtptime: u32) -> u64 {
    let dframes = u64::from(rtptime.wrapping_sub(anchor_rtptime));
    anchor_t_local + dframes * 1_000_000_000 / SAMPLE_RATE as u64
}

/// Drain all pending mirrored-volume updates (`--handoff`), returning the most
/// recent dBFS value (last-wins) or `None` if the channel was empty. Coalescing
/// to the newest avoids a backlog of stale `set_volume` calls if the user
/// sweeps the slider faster than the loop iterates.
fn drain_latest_volume(rx: &std::sync::mpsc::Receiver<f32>) -> Option<f32> {
    let mut latest = None;
    while let Ok(db) = rx.try_recv() {
        latest = Some(db);
    }
    latest
}

/// Drain all pending now-playing updates, returning only the most recent.
/// Track changes are rare, but coalescing keeps a burst from queueing several
/// round-trips on the RTSP control channel.
fn drain_latest_metadata(
    rx: &std::sync::mpsc::Receiver<NowPlaying>,
) -> Option<NowPlaying> {
    let mut latest = None;
    while let Ok(v) = rx.try_recv() {
        latest = Some(v);
    }
    latest
}

/// Push one now-playing update to a receiver.
///
/// Failures are logged and swallowed: a receiver that rejects metadata (or
/// artwork specifically — Shairport may not accept images) must keep playing
/// audio. The screen is never worth the stream.
fn send_metadata(r: &mut BufferedReceiver, np: &NowPlaying, rtptime: u32) {
    let dmap = openair_rtsp::dmap::encode_now_playing(&np.title, &np.artist, &np.album);
    // Log the exact bundle: the receiver answers 200 OK even when it declines to
    // display, so the wire bytes are the only way to tell a content problem from
    // a protocol one.
    debug!(
        receiver = %r.name,
        rtptime,
        title = %np.title,
        artist = %np.artist,
        album = %np.album,
        bytes = dmap.len(),
        dmap = %dmap.iter().map(|b| format!("{b:02x}")).collect::<String>(),
        "DMAP bundle"
    );
    if let Err(e) = r.session.set_metadata(&dmap, rtptime) {
        warn!(receiver = %r.name, "set_metadata failed (continuing): {e}");
    }
    if let Some((bytes, mime)) = &np.art {
        if let Err(e) = r.session.set_artwork(bytes, mime, rtptime) {
            warn!(receiver = %r.name, "set_artwork failed (continuing): {e}");
        }
    }
}

/// Push a receiver's effective volume: the group master plus that receiver's
/// trim, clamped to the protocol's usable range.
///
/// Failure is logged and swallowed — a receiver that rejects a volume change
/// must keep playing. Being at the wrong level is a nuisance; going silent is a
/// bug.
fn apply_volume(r: &mut BufferedReceiver, master_db: Option<f32>) {
    let Some(master) = master_db else { return };
    // -144 is the AirPlay "muted" sentinel; clamping to it means a deep trim
    // mutes rather than wrapping into nonsense.
    let effective = stats::effective_volume_db(master, r.trim_db);
    if let Err(e) = r.session.set_volume(effective) {
        warn!(receiver = %r.name, "set_volume failed (continuing): {e}");
    }
}

/// Apply one observer command to the live group.
///
/// Every arm follows the rule the codebase already uses for metadata: log the
/// failure, drop the receiver if it is unrecoverable, never take the stream
/// down. A UI control that misfires must not cost the user their audio.
#[allow(clippy::too_many_arguments)]
fn apply_command(
    cmd: stats::StreamCommand,
    group: &mut Vec<BufferedReceiver>,
    handles: &mut Vec<ReconnectHandle>,
    ptp: &PtpMaster,
    master_db: Option<f32>,
    anchor_t_local: u64,
    anchor_rtptime: u32,
    rtptime: u32,
) {
    use stats::{StreamCommand, TRIM_MAX_DB, TRIM_MIN_DB};

    match cmd {
        StreamCommand::SetTrim { addr, db } => {
            let db = db.clamp(TRIM_MIN_DB, TRIM_MAX_DB);
            // Update the pending reconnect too, so a receiver that is away
            // when the user trims it comes back at the level they chose.
            for h in handles.iter_mut().filter(|h| h.addr == addr) {
                h.trim_db = db;
            }
            let Some(r) = group.iter_mut().find(|r| r.addr == addr && r.alive) else {
                return;
            };
            r.trim_db = db;
            info!(receiver = %r.name, trim_db = db, "volume trim");
            apply_volume(r, master_db);
        }

        StreamCommand::SetOffset { addr, ms } => {
            for h in handles.iter_mut().filter(|h| h.addr == addr) {
                h.offset_ns = ms * 1_000_000;
            }
            let Some(r) = group.iter_mut().find(|r| r.addr == addr && r.alive) else {
                return;
            };
            r.offset_ns = ms * 1_000_000;
            // Re-state the group's schedule for this receiver alone, at the
            // current position, so its new offset takes effect without
            // disturbing anyone else's anchor.
            let play_at = play_deadline_ns(anchor_t_local, anchor_rtptime, rtptime);
            match anchor_receiver(ptp, r, play_at, rtptime) {
                Ok(()) => info!(receiver = %r.name, offset_ms = ms, "offset changed"),
                Err(e) => {
                    warn!(receiver = %r.name, "re-anchor after offset change failed — dropping: {e}");
                    r.alive = false;
                }
            }
        }

        StreamCommand::Remove { addr } => {
            // Cancel a pending reconnect for the same address, or the receiver
            // the user just removed would reappear moments later.
            handles.retain(|h| h.addr != addr);
            let Some(i) = group.iter().position(|r| r.addr == addr) else {
                return;
            };
            let mut gone = group.remove(i);
            info!(receiver = %gone.name, "removed from group");
            gone.finish();
        }

        StreamCommand::Add { addr, device_id } => {
            if group.iter().any(|r| r.addr == addr) || handles.iter().any(|h| h.addr == addr) {
                info!(%addr, "already in the group — ignoring add");
                return;
            }
            // Adding mid-stream is the same operation as recovering a dropped
            // receiver: prepare off-thread, then RECORD and anchor against the
            // live baseline when it is ready. Reusing that path means a new
            // receiver lands in sync by the same code that keeps a rejoining
            // one in sync.
            info!(%addr, "adding receiver to the group");
            handles.push(spawn_reconnect(addr, device_id, 0, 0.0, false));
        }
    }
}

/// Snapshot the group for an observer: the receivers currently streaming, plus
/// one entry per reconnect still in flight so a dropped receiver stays visible
/// rather than vanishing from the list while it recovers.
fn receiver_stats(
    group: &[BufferedReceiver],
    handles: &[ReconnectHandle],
    reconnect: bool,
) -> Vec<ReceiverStat> {
    let mut out: Vec<ReceiverStat> = group
        .iter()
        .map(|r| ReceiverStat {
            name: r.name.clone(),
            addr: r.addr,
            state: ReceiverState::Connected,
            offset_ms: r.offset_ns / 1_000_000,
            trim_db: r.trim_db,
        })
        .collect();

    for h in handles {
        // Without reconnect enabled (file playback) a gone receiver is gone.
        let state = if reconnect {
            ReceiverState::Reconnecting
        } else {
            ReceiverState::Dead
        };
        out.push(ReceiverStat {
            name: h.name.clone(),
            addr: h.addr,
            state,
            offset_ms: h.offset_ns / 1_000_000,
            trim_db: h.trim_db,
        });
    }
    out
}

/// Send the periodic `/feedback` keepalive to every live receiver every ~2 s
/// (also keeps a paused stream's session from timing out).
fn service_feedback(group: &mut [BufferedReceiver], last: &mut Instant) {
    if last.elapsed() >= Duration::from_secs(2) {
        for r in group.iter_mut() {
            if r.alive {
                if let Err(e) = r.session.feedback() {
                    warn!(receiver = %r.name, "feedback failed: {e}");
                }
            }
        }
        *last = Instant::now();
    }
}

/// Multi-room buffered streaming: the same AAC audio, time-synchronized, to
/// every receiver in `targets`.
///
/// How the group stays in sync: one PTP node serves the whole timing group,
/// and every session gets a SETRATEANCHORTIME for the SAME physical instant —
/// each expressed on the clock that receiver actually follows (ours for
/// Shairport, its own grandmaster for Apple) plus that receiver's user
/// offset. Each receiver plays frame N at the same wall-clock moment. Audio
/// is encoded once and encrypted per-receiver (each SETUP negotiates its own
/// AEAD key); per-receiver writer threads with bounded queues isolate a
/// stalling receiver from the rest of the group.
///
/// For live sources ([`AudioSource::is_live`]) a sustained silence pauses the
/// AirPlay stream (`rate=0`) and audio's return re-anchors and resumes it, so
/// pausing the music on the PC cleanly pauses/resumes every room.
///
/// If `volume_rx` is `Some` (the `--handoff` feature), mirrored-volume updates
/// (dBFS) drained from it each loop iteration are applied to every receiver,
/// overriding the initial `volume_db` seed from the first update onward.
pub fn stream_audio_buffered_multi(
    targets: &[GroupTarget],
    source: &mut dyn AudioSource,
    volume_db: Option<f32>,
    latency_ms: u64,
    volume_rx: Option<std::sync::mpsc::Receiver<f32>>,
    metadata_rx: Option<std::sync::mpsc::Receiver<NowPlaying>>,
    stats: Option<Arc<StreamStats>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if targets.is_empty() {
        return Err("no receivers given".into());
    }
    let group_ips: Vec<std::net::IpAddr> = targets.iter().map(|t| t.addr.ip()).collect();

    // One PTP node for the whole group, running before any receiver starts
    // monitoring us (and observing their masters before we anchor).
    let ptp = PtpMaster::start_multi(&group_ips)?;

    // --- Per-receiver RTSP negotiation ---
    let mut group: Vec<BufferedReceiver> = Vec::new();
    for target in targets {
        let name = format!("{}", target.addr);
        let setup = (|| -> Result<BufferedReceiver, Box<dyn std::error::Error>> {
            let control = ControlChannel::bind()?;
            let mut session = connect_session(target.addr, &target.device_id)?;
            let peer_ip = session.peer_ip();
            session.setup_timing(TimingConfig::Ptp)?;
            session.setup_stream(StreamFormat::AacBuffered, control.port)?;
            let event = open_event_channel(peer_ip, session.ports.event_port, session.event_keys());
            if let Err(e) = session.set_peers() {
                warn!("SETPEERS failed (continuing): {e}");
            }
            let cipher = AudioCipher::new(&session.shk);
            Ok(BufferedReceiver {
                name: name.clone(),
                addr: target.addr,
                device_id: target.device_id.clone(),
                session,
                cipher,
                offset_ns: target.offset_ms * 1_000_000,
                trim_db: 0.0,
                tx: None,
                writer: None,
                _event: event,
                _control: control,
                alive: true,
            })
        })();
        match setup {
            Ok(r) => group.push(r),
            Err(e) => {
                warn!(receiver = %name, "setup failed — skipping: {e}");
                // A half-open connection reset by the receiver usually means we
                // sourced it from the wrong interface; say so rather than
                // leaving a bare OS error code.
                if let Some(hint) = openair_core::net::connection_hint(target.addr.ip()) {
                    warn!(receiver = %name, "{hint}");
                }
            }
        }
    }
    if group.is_empty() {
        return Err("no receiver could be set up".into());
    }

    // Let every receiver's clock daemon converge before anchoring (nqptp
    // needs follow_ups to smooth; Apple clocks need offset samples from us).
    std::thread::sleep(Duration::from_millis(1500));

    // --- TCP audio connections + RECORD ---
    let mut seq: u32 = rand_seq() as u32;
    let first_rtptime: u32 = 0;
    for r in &mut group {
        let res = (|| -> Result<(), Box<dyn std::error::Error>> {
            let peer_ip = r.session.peer_ip();
            let data_stream =
                openair_core::net::connect_from_best_source(SocketAddr::new(peer_ip, r.session.ports.data_port))?;
            data_stream.set_nodelay(true).ok();
            r.session.record(seq as u16, first_rtptime)?;
            let (tx, handle) = spawn_writer(data_stream, r.name.clone());
            r.writer = Some(handle);
            r.tx = Some(tx);
            Ok(())
        })();
        if let Err(e) = res {
            warn!(receiver = %r.name, "connect/RECORD failed — dropping: {e}");
            r.alive = false;
        }
    }
    group.retain(|r| r.alive);
    if group.is_empty() {
        return Err("no receiver reached RECORD".into());
    }

    // --- Anchors: ONE shared physical instant (rtpTime=0 plays latency
    // from now), expressed per receiver on the timeline that receiver
    // actually follows plus its user offset. Same instant on every clock =
    // synchronized rooms, without relying on receivers seeing each other's
    // clocks. `current_latency` starts at the requested value and may be
    // auto-raised later if underruns are detected.
    let mut current_latency = latency_ms;
    let t_local = ptp_now_ns() + current_latency * 1_000_000;
    for r in &mut group {
        if let Err(e) = anchor_receiver(&ptp, r, t_local, first_rtptime) {
            warn!(receiver = %r.name, "anchor failed — dropping: {e}");
            r.alive = false;
        }
        apply_volume(r, volume_db);
    }
    group.retain(|r| r.alive);
    if group.is_empty() {
        return Err("no receiver accepted the anchor".into());
    }

    // --- Encode once + fan out (send-ahead pacing, with pause/resume) ---
    let live = source.is_live();
    // Only live (capture) streams reconnect: a dropped receiver mid-song is
    // worth chasing; a finite tone/file just loses it.
    let reconnect = live;
    let mut encoder = AacEncoder::new()?;
    // `pace_origin`/`frames_sent` are the wall-clock pacing baseline; both
    // reset on every resume so post-pause playback re-paces cleanly.
    let mut pace_origin = Instant::now();
    let mut frames_sent: i64 = 0;
    let mut last_feedback = Instant::now();

    // Current group anchor LINE: stream position `anchor_rtptime` is heard at
    // our-clock instant `anchor_t_local`. A receiver rejoining after a drop
    // anchors onto this same line so it lands in sync; the line is refreshed
    // on every resume. Because rtptime keeps advancing with wall clock (below),
    // this line stays valid even while the whole group is briefly empty.
    let mut anchor_t_local = t_local;
    let mut anchor_rtptime = first_rtptime;
    let mut handles: Vec<ReconnectHandle> = Vec::new();

    // Live volume, seeded by `volume_db` and overridden by `--handoff` mirror
    // updates. Tracked so rejoining receivers match the group's current level.
    let mut current_volume_db = volume_db;

    // Latest now-playing info, re-sent to receivers that rejoin after a drop so
    // a reconnecting room shows the current track instead of a blank screen.
    let mut current_metadata: Option<NowPlaying> = None;
    let mut last_metadata_send = Instant::now();

    // Auto-latency: track the minimum play-deadline lead over each window; if
    // it stays under the floor, step the latency up (bump-only, capped).
    let mut min_lead_ns: i64 = i64::MAX;
    let mut window_start = Instant::now();
    let mut last_bump = Instant::now();

    info!(receivers = group.len(), live, "streaming buffered AAC audio");

    let mut rtptime: u32 = first_rtptime;
    let mut paused = false;
    let mut silent_since: Option<Instant> = None;

    loop {
        // Rejoin any receivers whose background reconnect just succeeded, and
        // drop the handles of those that gave up.
        if !handles.is_empty() {
            let mut still = Vec::with_capacity(handles.len());
            for h in handles.drain(..) {
                match h.rx.try_recv() {
                    Ok(Ok(prep)) => {
                        if let Some(mut br) = finish_reconnect(
                            &ptp, prep, seq, rtptime, anchor_t_local, anchor_rtptime,
                            current_volume_db, paused,
                        ) {
                            // Bring the newcomer's screen up to date too.
                            if let Some(np) = &current_metadata {
                                send_metadata(&mut br, np, rtptime);
                            }
                            group.push(br);
                        }
                    }
                    Ok(Err(())) => {} // gave up (already logged)
                    Err(std::sync::mpsc::TryRecvError::Empty) => still.push(h),
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        warn!(receiver = %h.name, "reconnect thread vanished");
                    }
                }
            }
            handles = still;
        }

        // Nothing left to play to and nothing coming back → done.
        if group.is_empty() && handles.is_empty() {
            warn!("all receivers gone and no reconnects pending — stopping");
            break;
        }

        // Mirror Windows volume (--handoff): apply the latest update, if any, to
        // every live receiver. Done at the loop top so it still runs on the
        // paused/priming `continue` paths (a volume change while paused takes
        // effect on resume).
        if let Some(rx) = &volume_rx {
            if let Some(db) = drain_latest_volume(rx) {
                current_volume_db = Some(db);
                // Each receiver keeps its own trim, so moving the master
                // preserves the balance the user dialled in.
                for r in group.iter_mut() {
                    if r.alive {
                        apply_volume(r, Some(db));
                    }
                }
            }
        }

        // Commands from an observer (the TUI dashboard). Drained at the same
        // loop position as the volume mirror so they still land on the
        // paused/priming `continue` paths below.
        if let Some(s) = &stats {
            for cmd in s.drain_commands() {
                apply_command(
                    cmd,
                    &mut group,
                    &mut handles,
                    &ptp,
                    current_volume_db,
                    anchor_t_local,
                    anchor_rtptime,
                    rtptime,
                );
            }
        }

        // Now-playing metadata: same loop position as volume, so it still runs
        // on the paused/priming `continue` paths below.
        if let Some(rx) = &metadata_rx {
            if let Some(np) = drain_latest_metadata(rx) {
                info!(title = %np.title, artist = %np.artist, "sending now-playing metadata");
                for r in group.iter_mut() {
                    if r.alive {
                        send_metadata(r, &np, rtptime);
                    }
                }
                if let Some(s) = &stats {
                    s.set_now_playing(np.clone());
                }
                current_metadata = Some(np);
                last_metadata_send = Instant::now();
            } else if let Some(np) = &current_metadata {
                // Re-send periodically. The first send happens before a single
                // audio packet has gone out, and a receiver may reasonably
                // ignore metadata for a stream it hasn't started rendering.
                // Re-stating it once playback is established costs ~90 bytes.
                if last_metadata_send.elapsed() >= METADATA_RESEND_INTERVAL {
                    info!(title = %np.title, "re-sending now-playing metadata");
                    for r in group.iter_mut() {
                        if r.alive {
                            send_metadata(r, np, rtptime);
                        }
                    }
                    last_metadata_send = Instant::now();
                }
            }
        }

        // Send-ahead pacing (only while actively playing; a paused loop is
        // throttled by the blocking fill() below).
        if !paused {
            let elapsed_frames =
                (pace_origin.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as i64;
            if frames_sent - elapsed_frames >= BUFFERED_LEAD_SAMPLES {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        }

        let mut samples = [0i16; AAC_FRAMES_PER_PACKET * 2];
        let frames = source.fill(&mut samples);
        if frames == 0 {
            break; // source exhausted (EOF) or stopped (Ctrl+C)
        }
        if frames < AAC_FRAMES_PER_PACKET {
            // Zero-pad the final partial block.
            for v in &mut samples[frames * 2..] {
                *v = 0;
            }
        }

        // Pause/resume on sustained silence (live capture only: a quiet
        // passage in a file is music, not a pause).
        if live {
            let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
            if peak >= SILENCE_PEAK {
                silent_since = None;
                if paused {
                    // Audio's back: re-anchor at a fresh instant and resume.
                    info!("audio resumed — re-anchoring");
                    let t_local = ptp_now_ns() + current_latency * 1_000_000;
                    for r in &mut group {
                        if r.alive {
                            if let Err(e) = anchor_receiver(&ptp, r, t_local, rtptime) {
                                warn!(receiver = %r.name, "resume anchor failed — dropping: {e}");
                                r.alive = false;
                            }
                        }
                    }
                    reap_dead(&mut group, &mut handles, reconnect);
                    // Refresh the group anchor line so reconnects land on it.
                    anchor_t_local = t_local;
                    anchor_rtptime = rtptime;
                    paused = false;
                    pace_origin = Instant::now();
                    frames_sent = 0;
                }
            } else {
                let since = *silent_since.get_or_insert_with(Instant::now);
                if !paused && since.elapsed() >= PAUSE_AFTER_SILENCE {
                    info!("source silent — pausing AirPlay (rate=0)");
                    for r in &mut group {
                        if r.alive {
                            if let Err(e) = r.session.set_rate(0) {
                                warn!(receiver = %r.name, "pause set_rate(0) failed: {e}");
                            }
                        }
                    }
                    paused = true;
                }
            }
        }

        if paused {
            // Don't send audio while paused; fill() already drained the ring
            // and throttled the loop. Keep sessions alive with /feedback.
            service_feedback(&mut group, &mut last_feedback);
            continue;
        }

        // Encode + fan out only when we have receivers. While the group is
        // momentarily empty (all dropped, reconnects in flight) we skip the
        // encode but still advance the stream position below, so the anchor
        // line stays valid and a rejoining receiver lands in sync.
        if !group.is_empty() {
            let aac_frame = encoder.encode(&samples)?;
            if aac_frame.is_empty() {
                // Encoder still priming: no output yet, don't advance rtptime.
                continue;
            }
            for r in &mut group {
                if !r.alive {
                    continue;
                }
                let block = build_buffered_audio_block(
                    &mut r.cipher,
                    seq,
                    rtptime,
                    AAC_44100_F24_2_SSRC,
                    &aac_frame,
                );
                r.queue(block);
            }
            // Receivers that just dropped go to background reconnect.
            reap_dead(&mut group, &mut handles, reconnect);

            if let Some(s) = &stats {
                // One AAC frame goes to every receiver, so this is per-receiver
                // payload — the number a bandwidth reading should reflect.
                s.add_bytes(aac_frame.len() as u64);
                // Rebuilt here rather than inside `reap_dead` and the reconnect
                // path: this is the one place that sees the group after every
                // change, so the view cannot drift out of step with reality.
                s.set_receivers(receiver_stats(&group, &handles, reconnect));
            }

            // Auto-latency: how much headroom does the just-queued frame have
            // before its play deadline? Track the window minimum.
            let lead = play_deadline_ns(anchor_t_local, anchor_rtptime, rtptime) as i64
                - ptp_now_ns() as i64;
            min_lead_ns = min_lead_ns.min(lead);
            if let Some(s) = &stats {
                s.record_lead_ms(lead / 1_000_000);
            }

            if window_start.elapsed() >= AUTO_LATENCY_WINDOW {
                if min_lead_ns < UNDERRUN_LEAD_FLOOR.as_nanos() as i64
                    && current_latency < AUTO_LATENCY_MAX_MS
                    && last_bump.elapsed() >= AUTO_LATENCY_COOLDOWN
                {
                    let old = current_latency;
                    current_latency =
                        (current_latency + AUTO_LATENCY_STEP_MS).min(AUTO_LATENCY_MAX_MS);
                    warn!(
                        from_ms = old,
                        to_ms = current_latency,
                        min_lead_ms = min_lead_ns / 1_000_000,
                        "underrun risk — raising latency"
                    );
                    // Re-anchor the group deeper: current head plays
                    // `current_latency` from now, giving the receiver buffer
                    // room to refill.
                    let t_local = ptp_now_ns() + current_latency * 1_000_000;
                    for r in &mut group {
                        if r.alive {
                            if let Err(e) = anchor_receiver(&ptp, r, t_local, rtptime) {
                                warn!(receiver = %r.name, "auto-latency anchor failed — dropping: {e}");
                                r.alive = false;
                            }
                        }
                    }
                    reap_dead(&mut group, &mut handles, reconnect);
                    anchor_t_local = t_local;
                    anchor_rtptime = rtptime;
                    last_bump = Instant::now();
                    if let Some(s) = &stats {
                        s.set_latency_ms(current_latency);
                    }
                }
                min_lead_ns = i64::MAX;
                window_start = Instant::now();
            }
        }

        seq = seq.wrapping_add(1);
        rtptime = rtptime.wrapping_add(AAC_FRAMES_PER_PACKET as u32);
        frames_sent += AAC_FRAMES_PER_PACKET as i64;

        service_feedback(&mut group, &mut last_feedback);
    }

    // Wait for the queued audio to actually PLAY OUT before tearing down.
    // rtpTime advances with the send-ahead window, up to the whole lead ahead
    // of wall clock — tearing down immediately makes receivers dump the
    // unplayed tail (for a short source: ALL of it, silently). If we ended
    // while paused there is nothing buffered, so this naturally waits ~0.
    let played = Duration::from_secs_f64(frames_sent as f64 / SAMPLE_RATE as f64)
        + Duration::from_millis(current_latency + 250);
    let elapsed = pace_origin.elapsed();
    if played > elapsed {
        let wait = played - elapsed;
        info!(wait_ms = wait.as_millis() as u64, "draining playout before teardown");
        std::thread::sleep(wait);
    }

    info!("stream finished, tearing down");
    for r in &mut group {
        r.finish();
    }
    if let Some(s) = &stats {
        s.mark_ended();
    }
    Ok(())
}

/// Stream a sine tone to `addr` for `seconds`. Hardware smoke test for Step 4.
pub fn stream_tone(
    addr: SocketAddr,
    device_id: &str,
    seconds: u32,
    freq: f32,
    volume_db: Option<f32>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut source = SineSource::new(freq, seconds);
    stream_audio(addr, device_id, &mut source, volume_db)
}

fn rand_seq() -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
        & 0xFFFF) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_latest_volume_coalesces_to_newest() {
        let (tx, rx) = std::sync::mpsc::channel::<f32>();
        tx.send(-20.0).unwrap();
        tx.send(-12.0).unwrap();
        tx.send(-6.0).unwrap();
        assert_eq!(drain_latest_volume(&rx), Some(-6.0));
    }

    #[test]
    fn drain_latest_volume_empty_is_none() {
        let (_tx, rx) = std::sync::mpsc::channel::<f32>();
        assert_eq!(drain_latest_volume(&rx), None);
    }

    /// The real request the Apple TV sends, headers verbatim from a capture.
    fn sample_request(body_len: usize) -> Vec<u8> {
        let mut v = format!(
            "POST /command RTSP/1.0\r\nCSeq: 7\r\nContent-Length: {body_len}\r\n\
             Content-Type: application/x-apple-binary-plist\r\n\r\n"
        )
        .into_bytes();
        v.extend(std::iter::repeat_n(b'x', body_len));
        v
    }

    #[test]
    fn rtsp_message_len_waits_for_the_whole_body() {
        let full = sample_request(2414);
        // Header block alone is not a complete message.
        let head_only = header_block_end(&full).unwrap();
        assert_eq!(rtsp_message_len(&full[..head_only]), None);
        // One byte short is still incomplete — the real message spans 3 frames.
        assert_eq!(rtsp_message_len(&full[..full.len() - 1]), None);
        assert_eq!(rtsp_message_len(&full), Some(full.len()));
    }

    #[test]
    fn rtsp_message_len_none_until_headers_complete() {
        assert_eq!(rtsp_message_len(b"POST /command RTSP/1.0\r\nCSeq: 1"), None);
    }

    #[test]
    fn rtsp_message_len_handles_bodyless_request() {
        let msg = b"POST /command RTSP/1.0\r\nCSeq: 3\r\n\r\n";
        assert_eq!(rtsp_message_len(msg), Some(msg.len()));
    }

    #[test]
    fn event_response_echoes_cseq() {
        let resp = String::from_utf8(event_response(&sample_request(10))).unwrap();
        assert!(resp.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(resp.contains("CSeq: 7\r\n"), "CSeq must be echoed: {resp}");
        assert!(resp.ends_with("\r\n\r\n"));
    }

    #[test]
    fn header_value_is_case_insensitive() {
        let h = "POST / RTSP/1.0\r\ncontent-length: 42\r\nCSEQ: 9\r\n\r\n";
        assert_eq!(header_value(h, "Content-Length"), Some("42"));
        assert_eq!(header_value(h, "CSeq"), Some("9"));
        assert_eq!(header_value(h, "Missing"), None);
    }

    #[test]
    fn two_messages_in_one_buffer_are_split() {
        // Frames don't align to messages, so a buffer can hold more than one.
        let mut buf = sample_request(4);
        buf.extend(sample_request(6));
        let first = rtsp_message_len(&buf).unwrap();
        assert_eq!(first, sample_request(4).len());
        assert_eq!(rtsp_message_len(&buf[first..]), Some(sample_request(6).len()));
    }

    #[test]
    fn drain_latest_metadata_coalesces_to_newest() {
        let (tx, rx) = std::sync::mpsc::channel::<NowPlaying>();
        let mk = |t: &str| NowPlaying {
            title: t.into(),
            artist: "A".into(),
            album: "Al".into(),
            art: None,
        };
        tx.send(mk("first")).unwrap();
        tx.send(mk("second")).unwrap();
        assert_eq!(drain_latest_metadata(&rx).unwrap().title, "second");
        assert!(drain_latest_metadata(&rx).is_none());
    }

    #[test]
    fn drain_latest_volume_disconnected_returns_buffered_then_none() {
        let (tx, rx) = std::sync::mpsc::channel::<f32>();
        tx.send(-8.0).unwrap();
        drop(tx);
        // Buffered value is still delivered before the channel reads empty.
        assert_eq!(drain_latest_volume(&rx), Some(-8.0));
        assert_eq!(drain_latest_volume(&rx), None);
    }
}
