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

mod mediaremote;
mod pairings;
mod resample;
mod source;
mod stats;
pub use mediaremote::{set_media_handler_fn as set_media_handler, MediaCommand};
pub use pairings::{PairedPeer, PairingStore};
pub use source::{CaptureSource, SineSource, WavSource};
pub use stats::{
    buffer_health, ReceiverStat, ReceiverState, StreamCommand, StreamStats, TRIM_MAX_DB,
    TRIM_MIN_DB,
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

/// Whether this failure means "pair with it again" rather than "something
/// went wrong".
///
/// Worth distinguishing because the two look identical to a user staring at a
/// failed receiver, and only one of them has an action attached. A receiver
/// that has been factory reset, or had this sender removed from its Home,
/// will fail forever until it is paired again -- and nothing about a retry
/// loop hints at that.
pub fn needs_repairing(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = source {
        if matches!(
            err.downcast_ref::<openair_rtsp::SessionError>(),
            Some(openair_rtsp::SessionError::CredentialsRejected)
        ) {
            return true;
        }
        source = err.source();
    }
    false
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

// Message framing (`header_block_end`, `header_value`, `message_len`) lives in
// `openair_rtsp::message`. It used to live here too, in a second copy, and the
// two copies disagreed: this one reassembled a message across encrypted frames
// and the RTSP response reader did not. One implementation, so they cannot.
use openair_rtsp::message::{header_block_end, header_value, message_len as rtsp_message_len};

/// Build the RTSP response to an event-channel request.
///
/// The receiver only needs acknowledgement; it carries no body. `CSeq` must be
/// echoed back or the request is treated as unanswered.
/// Pull `flags=0x...` out of a receiver's event-channel message.
///
/// The message is a binary plist, but the receiver embeds its own mDNS TXT
/// record inside it as plain ASCII, so the value can be read without decoding
/// the plist. Returns `None` when absent, which is normal for messages that are
/// not `updateInfo`.
fn receiver_status_flags(body: &[u8]) -> Option<u64> {
    const NEEDLE: &[u8] = b"flags=0x";
    let start = body
        .windows(NEEDLE.len())
        .position(|w| w == NEEDLE)?
        + NEEDLE.len();
    let hex: String = body[start..]
        .iter()
        .take_while(|b| b.is_ascii_hexdigit())
        .map(|&b| b as char)
        .collect();
    u64::from_str_radix(&hex, 16).ok()
}

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
                    // The receiver's statusFlags, pulled straight out of the
                    // plist body. It embeds its mDNS TXT record as plain ASCII
                    // ("flags=0x120644"), so a substring scan gets it without
                    // decoding the bplist.
                    //
                    // Logged at INFO because it is the single most diagnostic
                    // value we receive: bit 0x100000 tracks whether the
                    // receiver has actually activated a session, and an Apple
                    // TV that never sets it shows no AirPlay UI at all. See
                    // DEVLOG session 19 / task #29.
                    if let Some(flags) = receiver_status_flags(&request) {
                        info!(
                            flags = %format!("{flags:#x}"),
                            session_active = flags & 0x10_0000 != 0,
                            "receiver status flags"
                        );
                    }
                    // Act on it before answering: the receiver is waiting on
                    // the reply, and a remote that responds only after the
                    // next round trip feels broken.
                    if let Some(cmd) = mediaremote::parse(&request) {
                        mediaremote::dispatch(cmd);
                    }
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
///
/// `name` is recorded alongside the credentials purely so they can be listed
/// and forgotten later by something a person recognises. Pass what the
/// receiver advertised.
pub fn pair_device(
    addr: SocketAddr,
    device_id: &str,
    name: Option<&str>,
    pin_provider: &mut dyn FnMut() -> String,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut store = PairingStore::load()?;
    // Persist the identity before pairing so a crash after M6 can't strand
    // an accessory that stored our LTPK we no longer have.
    store.ensure_saved()?;
    let identity = store.identity()?;
    let peer = openair_rtsp::pair_setup_normal(addr, device_id, &identity, pin_provider)?;
    store.set_peer(device_id, &peer, name)?;
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
    let control_handle = control.spawn_ptp(
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
    openair_core::qos::mark_ef(&audio_sock);
    // Held for the rest of the send loop: dropping it here would revert the
    // priority immediately and quietly do nothing at all.
    let _priority = openair_core::realtime::raise_current_thread();
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
    // Only the realtime path answers retransmits -- the buffered pipeline runs
    // over TCP, which does its own recovery -- so this is where the number
    // exists to report. Silent when the receiver never asked for anything,
    // which is the good case and does not need a line.
    if let Some(summary) = control_handle.retransmits().summary() {
        info!("{summary}");
    }
    session.set_rate(0).ok();
    session.teardown()?;
    Ok(())
}

/// Lead window (in samples) the buffered send loop tries to keep queued
/// ahead of wall-clock playback: while `frames_sent - elapsed_frames` is at
/// or above this, we sleep briefly instead of encoding/sending more.
const BUFFERED_LEAD_SAMPLES: i64 = 88_200; // 2s @ 44100 Hz

/// The send-ahead window for a given anchor latency, in samples.
///
/// Derived rather than constant. A hardcoded 2 s silently capped the headroom
/// at 2000 ms however deep the anchor was set, so raising the latency ceiling
/// without this would have produced a buffer that could never fill to the
/// number the user asked for.
///
/// A little beyond the latency itself, so the loop is not gated at exactly the
/// depth it is trying to hold — otherwise it spends every window on the
/// boundary, alternately sleeping and sending.
fn lead_samples_for(latency_ms: u64) -> i64 {
    let frames = (latency_ms as i64 * SAMPLE_RATE as i64) / 1000;
    (frames + frames / 10).max(BUFFERED_LEAD_SAMPLES / 4)
}
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
const PAUSE_AFTER_SILENCE: Duration = Duration::from_secs(30);

/// Why this is measured in tens of seconds rather than hundreds of
/// milliseconds.
///
/// Pausing tells every receiver `set_rate(0)` and resuming re-anchors the whole
/// group, which is an audible interruption and a resync risk. It is the right
/// thing when the user has actually stopped the music -- holding three rooms
/// hostage to a stream of digital silence is rude -- and the wrong thing for
/// any gap shorter than that.
///
/// At 1500 ms it fired on the gap *between tracks*: a three-hour dinner party
/// produced 37 pause/resume cycles, one per song change, each one stopping the
/// music in three rooms and re-anchoring on the way back. The cost of being
/// slow to notice a real stop is that receivers hold a silent stream a little
/// longer. The cost of being quick is breaking playback every few minutes.
const _: () = assert!(
    PAUSE_AFTER_SILENCE.as_secs() >= 10,
    "a threshold short enough to catch a track gap breaks playback every song"
);

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

/// Lowest latency a manual change may request. Mirrors the TUI's
/// `LATENCY_MIN_MS`; duplicated rather than imported because `client` must not
/// depend on `tui`. Below this the anchor sits inside one packet of audio and
/// the stream cannot stay ahead of itself.
const LATENCY_FLOOR_MS: u64 = 100;
/// Ceiling for auto-raised latency (a bump-only heuristic never lowers it).
const AUTO_LATENCY_MAX_MS: u64 = 4000;
/// Evaluation window: the minimum lead seen over this span is what's compared
/// against the floor, so a single transient dip can't ratchet latency up.
const AUTO_LATENCY_WINDOW: Duration = Duration::from_millis(1000);
/// Wait this long after a bump before considering another, so the deeper
/// buffer has time to fill and stabilise before we judge it again.
const AUTO_LATENCY_COOLDOWN: Duration = Duration::from_secs(5);

/// How often the buffer headroom is written at INFO. Every window would be
/// 3600 lines an hour on top of an already large log; every 30 s is enough to
/// see a slow decay while staying readable.
const LEAD_LOG_INTERVAL: Duration = Duration::from_secs(30);

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
#[derive(Clone)]
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
    /// Milliseconds of headroom at the last packet sent to this receiver.
    lead_ms: Option<i64>,
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
    openair_core::qos::mark_ef(&data_stream);
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
        lead_ms: None,
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
/// What to do at the end of an underrun window.
///
/// `Some(latency)` means re-anchor the group at that latency; `None` means
/// leave it alone.
///
/// Extracted from the send loop because of the bug it exists to prevent. The
/// re-anchor used to sit *inside* a `current_latency < AUTO_LATENCY_MAX_MS`
/// guard, so a user who set the latency to exactly the maximum got no recovery
/// at all: a brief network stall would eat the lead, nothing would give it
/// back, and successive stalls walked the headroom down through zero into
/// negative territory until the audio broke.
///
/// Raising the latency and recovering the lead are two different things.
/// Raising buys margin for *next* time; re-anchoring is what fixes *this*
/// time. Only the first has a ceiling.
fn underrun_response(min_lead_ns: i64, current_latency: u64, cooldown_ok: bool) -> Option<u64> {
    if min_lead_ns >= UNDERRUN_LEAD_FLOOR.as_nanos() as i64 || !cooldown_ok {
        return None;
    }
    Some((current_latency + AUTO_LATENCY_STEP_MS).min(AUTO_LATENCY_MAX_MS))
}

/// Re-anchor every live receiver so the current head plays `latency_ms` from
/// now, returning the new `anchor_t_local`.
///
/// Shared by auto-latency and by a manual latency change from the settings
/// overlay. Two code paths computing an anchor slightly differently is the kind
/// of divergence that produces a bug reproducible only one way round.
///
/// A receiver that cannot be re-anchored is marked dead; the caller is expected
/// to `reap_dead` afterwards.
fn re_anchor_group(
    ptp: &PtpMaster,
    group: &mut [BufferedReceiver],
    latency_ms: u64,
    rtptime: u32,
    why: &str,
) -> u64 {
    let t_local = ptp_now_ns() + latency_ms * 1_000_000;
    for r in group.iter_mut() {
        if r.alive {
            if let Err(e) = anchor_receiver(ptp, r, t_local, rtptime) {
                warn!(receiver = %r.name, "{why} anchor failed — dropping: {e}");
                r.alive = false;
            }
        }
    }
    t_local
}

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

/// Whether a metadata push should carry the cover art with it.
///
/// Worth a type rather than a bool at the call site: the text bundle is ~90
/// bytes and the artwork is 70-250 KB, sent as 1024-byte encrypted frames on
/// the same control channel the audio deadlines depend on. The difference
/// between the two is three orders of magnitude, and it should be impossible
/// to pick the wrong one by accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Artwork {
    Include,
    Skip,
}

/// Decides whether a given metadata push carries the cover art.
///
/// The rule is "at most twice per track": once when the track changes, and
/// once more on the first periodic re-send, because the first send happens
/// before a single audio packet has gone out and a receiver may reasonably
/// ignore metadata for a stream it has not started rendering.
///
/// A type rather than a loose bool so the invariant is checkable. The cost of
/// getting it wrong is not a wrong picture, it is megabytes of control-channel
/// traffic competing with the audio deadlines.
#[derive(Debug, Default)]
struct ArtworkSchedule {
    resent: bool,
}

impl ArtworkSchedule {
    /// A new track: send the art, and arm one restatement.
    fn track_changed(&mut self) -> Artwork {
        self.reset();
        Artwork::Include
    }

    /// The track changed but nothing was sent for it -- metadata is switched
    /// off.
    ///
    /// Still has to re-arm. Without this, turning metadata back on would send
    /// the new track's *text* and then decide its art had already gone out,
    /// leaving the receiver showing the previous track's cover until something
    /// else changed.
    fn reset(&mut self) {
        self.resent = false;
    }

    /// A periodic re-send of the same track.
    fn resend(&mut self) -> Artwork {
        if self.resent {
            return Artwork::Skip;
        }
        self.resent = true;
        Artwork::Include
    }
}

/// What the metadata tick should do this time round the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MetadataPush {
    /// Send nothing.
    None,
    /// Send the track that just arrived, with or without its art.
    Fresh(Artwork),
    /// Restate the track we are already on.
    Resend(Artwork),
}

/// Decide what this tick does, and advance the artwork schedule to match.
///
/// Pulled out of the loop so the decision can be tested. The interesting cases
/// are all about metadata being switched off and back on, which is exactly
/// where an in-loop version went wrong and where an in-loop test could not
/// reach.
fn plan_metadata(
    enabled: bool,
    new_track: bool,
    have_current: bool,
    resend_due: bool,
    schedule: &mut ArtworkSchedule,
) -> MetadataPush {
    if !enabled {
        // Off, but the world still moves. A track that arrives now is one
        // whose art has not been sent to anybody, so the schedule has to know
        // it is owed -- otherwise switching back on sends the new text under
        // the old cover.
        if new_track {
            schedule.reset();
        }
        return MetadataPush::None;
    }
    if new_track {
        return MetadataPush::Fresh(schedule.track_changed());
    }
    if have_current && resend_due {
        return MetadataPush::Resend(schedule.resend());
    }
    MetadataPush::None
}

/// Push one now-playing update to a receiver.
///
/// Failures are logged and swallowed: a receiver that rejects metadata (or
/// artwork specifically — Shairport may not accept images) must keep playing
/// audio. The screen is never worth the stream.
fn send_metadata(r: &mut BufferedReceiver, np: &NowPlaying, rtptime: u32, artwork: Artwork) {
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
    if artwork == Artwork::Skip {
        return;
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
        // Owned by the stream loop, which holds the anchor and the master
        // level. Routed here only if a caller bypasses that dispatch.
        StreamCommand::SetLatency { .. }
        | StreamCommand::SetMasterVolume { .. }
        | StreamCommand::SetMetadataEnabled { .. } => {
            debug_assert!(false, "a global command reached apply_command");
        }
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
    failed: &[ReceiverStat],
    latency_ms: u64,
) -> Vec<ReceiverStat> {
    let mut out: Vec<ReceiverStat> = group
        .iter()
        .map(|r| ReceiverStat {
            name: r.name.clone(),
            addr: r.addr,
            state: ReceiverState::Connected,
            offset_ms: r.offset_ns / 1_000_000,
            trim_db: r.trim_db,
            lead_ms: r.lead_ms,
            health: r
                .lead_ms
                .map(|ms| stats::buffer_health(ms, latency_ms))
                .unwrap_or(0.0),
            error: None,
            needs_pairing: false,
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
            lead_ms: None,
            health: 0.0,
            error: None,
            needs_pairing: false,
        });
    }

    // Failures are carried forward explicitly. This function rebuilds from the
    // live group, which by definition never contained a receiver that failed to
    // connect — without merging them back a failed receiver would vanish from
    // the UI on the very next snapshot, which is exactly when the user is
    // looking for it.
    //
    // A later success for the same address wins: a retried receiver is in
    // `group` now, and showing it as both connected and failed would be worse
    // than either.
    for f in failed {
        if !out.iter().any(|r| r.addr == f.addr) {
            out.push(f.clone());
        }
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
    // This function *is* the pacing loop: it wakes roughly every 23 ms to send
    // a frame with a play deadline attached, for the whole life of the stream.
    // Held to the end of the function, so the priority lasts as long as the
    // sending does.
    let _priority = openair_core::realtime::raise_current_thread();
    let group_ips: Vec<std::net::IpAddr> = targets.iter().map(|t| t.addr.ip()).collect();

    // One PTP node for the whole group, running before any receiver starts
    // monitoring us (and observing their masters before we anchor).
    let ptp = PtpMaster::start_multi(&group_ips)?;

    // --- Per-receiver RTSP negotiation ---
    // Every target starts as `Connecting` so the UI can show the whole group
    // immediately, rather than receivers popping into the list one at a time as
    // each handshake finishes.
    let mut progress: Vec<ReceiverStat> = targets
        .iter()
        .map(|t| ReceiverStat {
            name: format!("{}", t.addr),
            addr: t.addr,
            state: ReceiverState::Connecting,
            offset_ms: t.offset_ms,
            trim_db: 0.0,
            lead_ms: None,
            health: 0.0,
            error: None,
            needs_pairing: false,
        })
        .collect();
    if let Some(s) = &stats {
        s.set_receivers(progress.clone());
    }

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
                lead_ms: None,
                trim_db: 0.0,
                tx: None,
                writer: None,
                _event: event,
                _control: control,
                alive: true,
            })
        })();
        match setup {
            Ok(r) => {
                if let Some(p) = progress.iter_mut().find(|p| p.addr == target.addr) {
                    p.state = ReceiverState::Connected;
                }
                group.push(r);
            }
            Err(e) => {
                warn!(receiver = %name, "setup failed — skipping: {e}");
                // Stored credentials the receiver no longer honours: it was
                // reset, or we were removed from its Home. Retrying cannot fix
                // that and neither can any hint about interfaces, so it is
                // reported as itself and nothing else is guessed at.
                let repair = needs_repairing(e.as_ref());
                // A half-open connection reset by the receiver usually means we
                // sourced it from the wrong interface; say so rather than
                // leaving a bare OS error code.
                let hint = if repair {
                    None
                } else {
                    openair_core::net::connection_hint(target.addr.ip())
                };
                if let Some(hint) = &hint {
                    warn!(receiver = %name, "{hint}");
                }
                if let Some(p) = progress.iter_mut().find(|p| p.addr == target.addr) {
                    p.state = ReceiverState::Failed;
                    // Prefer the actionable hint over the raw error: "10054"
                    // tells the user nothing, "try --bind <ip>" tells them what
                    // to do. The raw error is already in the log above.
                    p.error = Some(hint.unwrap_or_else(|| e.to_string()));
                    p.needs_pairing = repair;
                }
            }
        }
        if let Some(s) = &stats {
            s.set_receivers(progress.clone());
        }
    }

    // Carried for the rest of the run so a receiver that never connected stays
    // visible instead of silently disappearing.
    let failed: Vec<ReceiverStat> = progress
        .iter()
        .filter(|p| p.state == ReceiverState::Failed)
        .cloned()
        .collect();
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
            openair_core::qos::mark_ef(&data_stream);
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
    // Gates transmission only; the watcher upstream keeps running either way.
    let mut metadata_enabled = metadata_rx.is_some();
    let mut last_metadata_send = Instant::now();
    let mut art_schedule = ArtworkSchedule::default();

    // Auto-latency: track the minimum play-deadline lead over each window; if
    // it stays under the floor, step the latency up (bump-only, capped).
    let mut min_lead_ns: i64 = i64::MAX;
    let mut window_start = Instant::now();
    let mut last_bump = Instant::now();
    let mut last_lead_log = Instant::now();

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
                                // A newcomer has never seen this track, so it
                                // gets the art as well as the text.
                                send_metadata(&mut br, np, rtptime, Artwork::Include);
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
                match cmd {
                    stats::StreamCommand::SetLatency { ms } => {
                        let ms = ms.clamp(LATENCY_FLOOR_MS, AUTO_LATENCY_MAX_MS);
                        if ms != current_latency {
                            info!(from_ms = current_latency, to_ms = ms, "latency changed");
                            current_latency = ms;
                            anchor_t_local = re_anchor_group(
                                &ptp,
                                &mut group,
                                current_latency,
                                rtptime,
                                "latency change",
                            );
                            anchor_rtptime = rtptime;
                            // Count a manual change as a bump, so auto-latency
                            // does not immediately step on top of a value the
                            // user just chose.
                            last_bump = Instant::now();
                            reap_dead(&mut group, &mut handles, reconnect);
                            s.set_latency_ms(current_latency);
                        }
                    }
                    stats::StreamCommand::SetMasterVolume { db } => {
                        current_volume_db = Some(db);
                        for r in group.iter_mut() {
                            if r.alive {
                                let level = stats::effective_volume_db(db, r.trim_db);
                                if let Err(e) = r.session.set_volume(level) {
                                    warn!(receiver = %r.name, "set_volume failed (continuing): {e}");
                                }
                            }
                        }
                    }
                    stats::StreamCommand::SetMetadataEnabled { on } => {
                        info!(enabled = on, "now-playing metadata toggled");
                        metadata_enabled = on;
                    }
                    other => apply_command(
                        other,
                        &mut group,
                        &mut handles,
                        &ptp,
                        current_volume_db,
                        anchor_t_local,
                        anchor_rtptime,
                        rtptime,
                    ),
                }
            }
        }

        // Now-playing metadata: same loop position as volume, so it still runs
        // on the paused/priming `continue` paths below.
        if let Some(rx) = &metadata_rx {
            // Drained even while switched off, so the channel does not back up
            // and switching back on sends the *current* track rather than
            // replaying a queue. The watcher itself keeps running; only
            // transmission stops.
            let latest = drain_latest_metadata(rx);
            let plan = plan_metadata(
                metadata_enabled,
                latest.is_some(),
                current_metadata.is_some(),
                last_metadata_send.elapsed() >= METADATA_RESEND_INTERVAL,
                &mut art_schedule,
            );
            if let Some(np) = latest {
                current_metadata = Some(np);
            }
            if let MetadataPush::Fresh(artwork) = plan {
                let np = current_metadata
                    .as_ref()
                    .expect("Fresh implies a track just arrived");
                info!(title = %np.title, artist = %np.artist, "sending now-playing metadata");
                for r in group.iter_mut() {
                    if r.alive {
                        send_metadata(r, np, rtptime, artwork);
                    }
                }
                if let Some(s) = &stats {
                    s.set_now_playing(np.clone());
                }
                last_metadata_send = Instant::now();
            } else if let (MetadataPush::Resend(artwork), Some(np)) = (plan, &current_metadata) {
                // Re-send periodically. The first send happens before a single
                // audio packet has gone out, and a receiver may reasonably
                // ignore metadata for a stream it hasn't started rendering.
                //
                // The art goes out on the *first* re-send only, for the same
                // reason the text does, and never again for this track. Every
                // later tick is the ~90-byte text bundle.
                //
                // It used to carry the art every time: 70-250 KB, every ten
                // seconds, in 1024-byte encrypted frames on the same control
                // channel the audio deadlines run through. A four-minute track
                // spent megabytes restating a picture the receiver already had.
                info!(title = %np.title, ?artwork, "re-sending now-playing metadata");
                for r in group.iter_mut() {
                    if r.alive {
                        send_metadata(r, np, rtptime, artwork);
                    }
                }
                last_metadata_send = Instant::now();
            }
        }

        // Send-ahead pacing (only while actively playing; a paused loop is
        // throttled by the blocking fill() below).
        if !paused {
            let elapsed_frames =
                (pace_origin.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as i64;
            if frames_sent - elapsed_frames >= lead_samples_for(current_latency) {
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
                // Total payload actually put on the wire: the same AAC frame is
                // sent to every live receiver, so three rooms cost three times
                // one room. Reporting a single receiver's share made the
                // dashboard read a third of reality on a three-room group,
                // which is not what anyone comparing against their network
                // monitor expects.
                let alive = group.iter().filter(|r| r.alive).count().max(1) as u64;
                s.add_bytes(aac_frame.len() as u64 * alive);
                // Rebuilt here rather than inside `reap_dead` and the reconnect
                // path: this is the one place that sees the group after every
                // change, so the view cannot drift out of step with reality.
                s.set_receivers(receiver_stats(
                    &group,
                    &handles,
                    reconnect,
                    &failed,
                    current_latency,
                ));
            }

            // Auto-latency: how much headroom does the just-queued frame have
            // before its play deadline? Track the window minimum.
            let deadline = play_deadline_ns(anchor_t_local, anchor_rtptime, rtptime) as i64;
            let now = ptp_now_ns() as i64;
            let lead = deadline - now;
            min_lead_ns = min_lead_ns.min(lead);
            if let Some(s) = &stats {
                s.record_lead_ms(lead / 1_000_000);
            }
            // The group shares one anchor line, but each receiver plays at its
            // own offset, so their deadlines differ. Recorded per receiver so a
            // UI can show which room is running dry rather than only that the
            // group is.
            for r in &mut group {
                if r.alive {
                    r.lead_ms = Some((lead + r.offset_ns) / 1_000_000);
                }
            }

            if window_start.elapsed() >= AUTO_LATENCY_WINDOW {
                let cooldown_ok = last_bump.elapsed() >= AUTO_LATENCY_COOLDOWN;
                if let Some(next_latency) =
                    underrun_response(min_lead_ns, current_latency, cooldown_ok)
                {
                    let old = current_latency;
                    current_latency = next_latency;
                    if current_latency > old {
                        warn!(
                            from_ms = old,
                            to_ms = current_latency,
                            min_lead_ms = min_lead_ns / 1_000_000,
                            "underrun risk — raising latency"
                        );
                    } else {
                        // Already at the ceiling. Re-anchoring still matters —
                        // it is what gives back the lead a stall consumed.
                        warn!(
                            latency_ms = current_latency,
                            min_lead_ms = min_lead_ns / 1_000_000,
                            "underrun risk at maximum latency — re-anchoring"
                        );
                    }
                    // Re-anchor the group deeper: current head plays
                    // `current_latency` from now, giving the receiver buffer
                    // room to refill. Unconditional, unlike the raise above.
                    let t_local =
                        re_anchor_group(&ptp, &mut group, current_latency, rtptime, "auto-latency");
                    reap_dead(&mut group, &mut handles, reconnect);
                    anchor_t_local = t_local;
                    anchor_rtptime = rtptime;
                    last_bump = Instant::now();
                    if let Some(s) = &stats {
                        s.set_latency_ms(current_latency);
                    }
                }
                // The number the dashboard shows as buffer headroom, which
                // until now existed only on screen: a multi-hour log of a
                // session where it visibly decayed to negative could not say
                // so, because nothing ever wrote it down.
                let lead_ms = min_lead_ns / 1_000_000;
                if last_lead_log.elapsed() >= LEAD_LOG_INTERVAL {
                    info!(
                        min_lead_ms = lead_ms,
                        latency_ms = current_latency,
                        receivers = group.iter().filter(|r| r.alive).count(),
                        "buffer headroom"
                    );
                    last_lead_log = Instant::now();
                } else {
                    debug!(min_lead_ms = lead_ms, "buffer headroom");
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

    fn failed_stat(addr: &str, why: &str) -> ReceiverStat {
        ReceiverStat {
            name: addr.to_string(),
            addr: addr.parse().unwrap(),
            state: ReceiverState::Failed,
            offset_ms: 0,
            trim_db: 0.0,
            lead_ms: None,
            health: 0.0,
            error: Some(why.to_string()),
            needs_pairing: false,
        }
    }

    /// An error that wraps another, to prove the walk reaches the bottom.
    #[derive(Debug)]
    struct Wrapped(Box<dyn std::error::Error + 'static>);

    impl std::fmt::Display for Wrapped {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "while setting up the session")
        }
    }

    impl std::error::Error for Wrapped {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(self.0.as_ref())
        }
    }

    /// The tick, driven the way the loop drives it.
    ///
    /// Tests go through `plan_metadata` rather than `ArtworkSchedule` directly:
    /// the bug this replaced lived in *when the schedule was advanced*, not in
    /// the schedule, and a test of the type alone passed happily while the call
    /// site was wrong.
    struct Ticker {
        schedule: ArtworkSchedule,
        enabled: bool,
        have_current: bool,
    }

    impl Ticker {
        fn new() -> Self {
            Self {
                schedule: ArtworkSchedule::default(),
                enabled: true,
                have_current: false,
            }
        }

        /// A track change arrives.
        fn track(&mut self) -> MetadataPush {
            self.have_current = true;
            plan_metadata(self.enabled, true, true, false, &mut self.schedule)
        }

        /// The re-send interval elapses with no new track.
        fn tick(&mut self) -> MetadataPush {
            plan_metadata(self.enabled, false, self.have_current, true, &mut self.schedule)
        }

        /// A tick before the interval is up.
        fn idle(&mut self) -> MetadataPush {
            plan_metadata(self.enabled, false, self.have_current, false, &mut self.schedule)
        }
    }

    #[test]
    fn a_new_track_sends_its_art() {
        assert_eq!(Ticker::new().track(), MetadataPush::Fresh(Artwork::Include));
    }

    #[test]
    fn the_art_is_restated_once_and_then_left_alone() {
        // Once, because the first send happens before playback has started and
        // may be ignored. Not twice, because the receiver has it by then.
        let mut t = Ticker::new();
        t.track();
        assert_eq!(t.tick(), MetadataPush::Resend(Artwork::Include));
        for _ in 0..100 {
            assert_eq!(t.tick(), MetadataPush::Resend(Artwork::Skip));
        }
    }

    #[test]
    fn art_goes_out_at_most_twice_per_track() {
        // The invariant the type exists for. At ten seconds a tick and up to
        // 250 KB a picture, an off-by-one here is megabytes per track on the
        // same channel the audio deadlines run through.
        let mut t = Ticker::new();
        let mut sent = 0;
        if t.track() == MetadataPush::Fresh(Artwork::Include) {
            sent += 1;
        }
        // A ten-minute track at one tick every ten seconds.
        for _ in 0..60 {
            if t.tick() == MetadataPush::Resend(Artwork::Include) {
                sent += 1;
            }
        }
        assert_eq!(sent, 2, "art was sent {sent} times over one track");
    }

    #[test]
    fn a_track_change_arms_one_more_restatement() {
        let mut t = Ticker::new();
        t.track();
        t.tick();
        assert_eq!(t.tick(), MetadataPush::Resend(Artwork::Skip));

        assert_eq!(t.track(), MetadataPush::Fresh(Artwork::Include));
        assert_eq!(t.tick(), MetadataPush::Resend(Artwork::Include));
        assert_eq!(t.tick(), MetadataPush::Resend(Artwork::Skip));
    }

    #[test]
    fn a_track_that_arrived_while_muted_still_gets_its_art_later() {
        // Regression. Turning metadata off, letting the track change, and
        // turning it back on left the new track's art unsent: the schedule
        // still believed it had restated the *previous* track, so the receiver
        // showed the old cover until something else changed.
        let mut t = Ticker::new();
        t.track();
        t.tick();
        assert_eq!(t.tick(), MetadataPush::Resend(Artwork::Skip));

        t.enabled = false;
        assert_eq!(t.track(), MetadataPush::None, "nothing goes out while off");

        t.enabled = true;
        assert_eq!(
            t.tick(),
            MetadataPush::Resend(Artwork::Include),
            "the track that arrived while muted is still owed its art"
        );
    }

    #[test]
    fn nothing_is_sent_while_metadata_is_switched_off() {
        let mut t = Ticker::new();
        t.enabled = false;
        assert_eq!(t.track(), MetadataPush::None);
        assert_eq!(t.tick(), MetadataPush::None);
    }

    #[test]
    fn nothing_is_resent_before_the_interval_is_up() {
        let mut t = Ticker::new();
        t.track();
        assert_eq!(t.idle(), MetadataPush::None);
    }

    #[test]
    fn nothing_is_resent_before_a_first_track_exists() {
        // There is nothing to restate, and unwrapping a track we do not have
        // would panic the stream thread.
        let mut t = Ticker::new();
        assert_eq!(t.tick(), MetadataPush::None);
    }

    #[test]
    fn a_rejected_pairing_is_recognised() {
        let e = openair_rtsp::SessionError::CredentialsRejected;
        assert!(needs_repairing(&e));
    }

    #[test]
    fn a_rejected_pairing_is_recognised_through_a_wrapper() {
        // The real call site sees whatever the setup closure boxed up, which is
        // rarely the bare error -- a check that only handled the top of the
        // chain would pass its unit test and never fire in practice.
        let inner = Box::new(openair_rtsp::SessionError::CredentialsRejected);
        let outer = Wrapped(Box::new(Wrapped(inner)));
        assert!(needs_repairing(&outer));
    }

    #[test]
    fn ordinary_failures_do_not_ask_for_re_pairing() {
        // Offering to re-pair a receiver that is merely unreachable sends the
        // user to type a PIN off a screen that will never show one.
        for e in [
            openair_rtsp::SessionError::Http(500),
            openair_rtsp::SessionError::EmptyResponse,
            // A device asking for on-screen approval is a different remedy:
            // approve it there, not pair again.
            openair_rtsp::SessionError::AuthorizationRequired,
        ] {
            assert!(!needs_repairing(&e), "{e} should not ask for re-pairing");
        }
        let io_error = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        assert!(!needs_repairing(&io_error));
    }

    #[test]
    fn failed_receivers_survive_a_snapshot_rebuild() {
        // receiver_stats rebuilds from the live group, which by definition
        // never held a receiver that failed to connect. Without the merge a
        // failure would vanish on the next packet — precisely when the user is
        // reading the list to find out what went wrong.
        let failed = vec![failed_stat("192.168.1.51:7000", "try --bind 192.168.1.10")];
        let out = receiver_stats(&[], &[], true, &failed, 500);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].state, ReceiverState::Failed);
        assert_eq!(out[0].error.as_deref(), Some("try --bind 192.168.1.10"));
    }

    #[test]
    fn a_reconnecting_receiver_is_not_duplicated_by_a_stale_failure() {
        // After a retry the receiver is pending again, so the failed entry for
        // the same address must not also be listed.
        let addr: SocketAddr = "192.168.1.51:7000".parse().unwrap();
        let handles = vec![ReconnectHandle {
            name: "Pool Room".into(),
            trim_db: 0.0,
            addr,
            offset_ns: 0,
            rx: std::sync::mpsc::channel().1,
        }];
        let failed = vec![failed_stat("192.168.1.51:7000", "connection refused")];

        let out = receiver_stats(&[], &handles, true, &failed, 500);
        assert_eq!(out.len(), 1, "one row per receiver, not one per state");
        assert_eq!(out[0].state, ReceiverState::Reconnecting);
    }

    #[test]
    fn failures_for_other_addresses_are_kept_alongside() {
        let addr: SocketAddr = "192.168.1.51:7000".parse().unwrap();
        let handles = vec![ReconnectHandle {
            name: "Pool Room".into(),
            trim_db: 0.0,
            addr,
            offset_ns: 0,
            rx: std::sync::mpsc::channel().1,
        }];
        let failed = vec![failed_stat("192.168.1.88:7000", "no route to host")];

        let out = receiver_stats(&[], &handles, true, &failed, 500);
        assert_eq!(out.len(), 2);
        assert!(out.iter().any(|r| r.state == ReceiverState::Failed));
        assert!(out.iter().any(|r| r.state == ReceiverState::Reconnecting));
    }

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
    fn event_response_echoes_cseq() {
        let resp = String::from_utf8(event_response(&sample_request(10))).unwrap();
        assert!(resp.starts_with("RTSP/1.0 200 OK\r\n"));
        assert!(resp.contains("CSeq: 7\r\n"), "CSeq must be echoed: {resp}");
        assert!(resp.ends_with("\r\n\r\n"));
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

    #[test]
    fn status_flags_are_read_out_of_a_plist_body() {
        // Real shape: the flag string is embedded in binary plist noise, so the
        // scan has to survive non-UTF-8 either side of it.
        let mut body = vec![0x00, 0xff, 0xfe, b'b', b'p', b'l', b'i', b's', b't'];
        body.extend_from_slice(b"gid=X(flags=0x120644(igl=1");
        body.extend_from_slice(&[0xff, 0x00]);
        let flags = receiver_status_flags(&body).unwrap();
        assert_eq!(flags, 0x12_0644);
        assert!(flags & 0x10_0000 != 0, "session-active bit");
    }

    #[test]
    fn the_inactive_flag_value_reads_as_inactive() {
        // The value seen on every run where the Apple TV showed no UI.
        let body = b"flags=0x20644(igl=1".to_vec();
        let flags = receiver_status_flags(&body).unwrap();
        assert_eq!(flags, 0x2_0644);
        assert!(flags & 0x10_0000 == 0, "not active");
    }

    #[test]
    fn a_message_without_flags_yields_none() {
        assert!(receiver_status_flags(b"no flags here at all").is_none());
        assert!(receiver_status_flags(b"").is_none());
    }

    #[test]
    fn a_truncated_flag_value_does_not_panic() {
        // The scan runs on whatever the receiver sent; a cut-off message must
        // not take the event thread down.
        assert!(receiver_status_flags(b"flags=0x").is_none());
        assert_eq!(receiver_status_flags(b"flags=0x1").unwrap(), 1);
    }


    #[test]
    fn an_underrun_at_maximum_latency_still_re_anchors() {
        // The bug from the 3-room dinner-party session: latency set to exactly
        // AUTO_LATENCY_MAX_MS meant the recovery block never ran, so a brief
        // stall permanently ate the lead and successive stalls walked the
        // headroom to -400 ms.
        let starved = 0;
        let got = underrun_response(starved, AUTO_LATENCY_MAX_MS, true);
        assert_eq!(
            got,
            Some(AUTO_LATENCY_MAX_MS),
            "must still re-anchor at the ceiling, just without raising"
        );
    }

    #[test]
    fn an_underrun_below_maximum_raises_and_re_anchors() {
        let starved = 0;
        let got = underrun_response(starved, 500, true).expect("should respond");
        assert_eq!(got, 500 + AUTO_LATENCY_STEP_MS);
    }

    #[test]
    fn the_raise_is_capped_but_the_response_is_not_suppressed() {
        let starved = 0;
        let near = AUTO_LATENCY_MAX_MS - 10;
        assert_eq!(
            underrun_response(starved, near, true),
            Some(AUTO_LATENCY_MAX_MS),
            "clamped to the ceiling rather than overshooting"
        );
    }

    #[test]
    fn healthy_headroom_does_nothing() {
        let healthy = UNDERRUN_LEAD_FLOOR.as_nanos() as i64 * 4;
        assert_eq!(underrun_response(healthy, 500, true), None);
        assert_eq!(underrun_response(healthy, AUTO_LATENCY_MAX_MS, true), None);
    }

    #[test]
    fn the_cooldown_suppresses_a_response() {
        // Re-anchoring every window would be worse than the problem: each one
        // interrupts playback timing on every receiver.
        let starved = 0;
        assert_eq!(underrun_response(starved, 500, false), None);
        assert_eq!(underrun_response(starved, AUTO_LATENCY_MAX_MS, false), None);
    }

    #[test]
    fn a_negative_lead_is_treated_as_starved() {
        // Headroom went to -400 ms in the reported session; that is the most
        // urgent case, not an edge one.
        assert!(underrun_response(-400_000_000, AUTO_LATENCY_MAX_MS, true).is_some());
    }


    #[test]
    fn the_send_ahead_window_follows_the_latency() {
        // A hardcoded 2 s capped headroom at 2000 ms no matter how deep the
        // anchor was set, so a 3 s buffer could never actually fill.
        let two_s = lead_samples_for(2000);
        let three_s = lead_samples_for(3000);
        assert!(three_s > two_s, "{three_s} should exceed {two_s}");
        assert!(
            two_s >= 88_200,
            "must not gate below the latency it is holding"
        );
    }

    #[test]
    fn a_tiny_latency_still_leaves_a_usable_window() {
        // Gating at nearly zero would make the loop sleep and wake constantly.
        assert!(lead_samples_for(0) > 0);
        assert!(lead_samples_for(100) > 0);
    }

}
