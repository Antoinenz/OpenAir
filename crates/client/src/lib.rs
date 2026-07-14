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
use openair_rtsp::{StreamFormat, StreamSession, TimingConfig};
use openair_timing::{ptp_now_ns, ptp_ns_to_secs_frac, PtpMaster};
use tracing::{info, warn};

mod source;
pub use source::{CaptureSource, SineSource, WavSource};

pub(crate) const SAMPLE_RATE: u32 = 44100;

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
    let mut session = StreamSession::connect(addr, device_id)?;
    let peer_ip = session.peer_ip();

    // PTP master must be running before the receiver starts monitoring us.
    let ptp = PtpMaster::start(peer_ip)?;

    session.setup_timing(TimingConfig::Ptp)?;
    session.setup_stream(StreamFormat::AlacRealtime, control_port)?;
    let ports = session.ports;

    // Let the receiver's clock daemon converge on our PTP clock before audio
    // starts: nqptp resets its clock records at SETUP and its offset
    // smoothing needs ~1-2s of follow_ups; starting audio immediately causes
    // audible resync churn in the first seconds.
    std::thread::sleep(Duration::from_millis(1500));

    // Shared clock state for the control thread. t0 = PTP time of frame 0;
    // all anchor packets extrapolate from it (collinear anchor line).
    let t0_ns = ptp_now_ns();
    let state = Arc::new(SyncState {
        head_ts: std::sync::atomic::AtomicU64::new(0),
        start_ts: std::sync::atomic::AtomicU64::new(0),
        latency: std::sync::atomic::AtomicU64::new(0),
        t0_ns: std::sync::atomic::AtomicU64::new(t0_ns),
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

    // rate=1 flips ap2_play_enabled on the receiver; the realtime anchor
    // itself comes from the control-channel type-215 packets.
    session.set_rate(1)?;

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
    // --- Idle control port (SETUP requires one; buffered streams anchor via RTSP) ---
    let control = ControlChannel::bind()?;
    let control_port = control.port;

    // --- RTSP negotiation ---
    let mut session = StreamSession::connect(addr, device_id)?;
    let peer_ip = session.peer_ip();

    // PTP master must run for the whole session, started before SETUP.
    let ptp = PtpMaster::start(peer_ip)?;

    session.setup_timing(TimingConfig::Ptp)?;
    session.setup_stream(StreamFormat::AacBuffered, control_port)?;
    let ports = session.ports;

    // Let the receiver's clock daemon converge on our PTP clock before
    // anchoring (see stream_audio for rationale).
    std::thread::sleep(Duration::from_millis(1500));

    // --- TCP audio data connection ---
    let mut data_stream = TcpStream::connect(SocketAddr::new(peer_ip, ports.data_port))?;
    data_stream.set_nodelay(true).ok();

    // --- RECORD ---
    let mut seq: u32 = rand_seq() as u32;
    let first_rtptime: u32 = 0;
    session.record(seq as u16, first_rtptime)?;

    // --- Anchor: full SETRATEANCHORTIME, rtpTime=0 plays latency_ms from now ---
    let anchor_ns = ptp_now_ns() + latency_ms * 1_000_000;
    let (anchor_secs, anchor_frac) = ptp_ns_to_secs_frac(anchor_ns);
    session.set_rate_anchor(ptp.clock_id, first_rtptime, anchor_secs, anchor_frac, 1)?;

    if let Some(db) = volume_db {
        if let Err(e) = session.set_volume(db) {
            warn!("set_volume failed (continuing): {e}");
        }
    }

    // --- Encode + send loop (send-ahead pacing) ---
    let mut cipher = AudioCipher::new(&session.shk);
    let mut encoder = AacEncoder::new()?;

    let start_instant = Instant::now();
    let mut last_feedback = Instant::now();

    info!(data_port = ports.data_port, "streaming buffered AAC audio");

    let mut rtptime: u32 = first_rtptime;
    let mut frames_sent: i64 = 0;
    let mut source_ended = false;

    loop {
        // Send-ahead pacing: if we're too far ahead of wall-clock playback,
        // sleep instead of encoding/sending more.
        let elapsed_frames = (start_instant.elapsed().as_secs_f64() * SAMPLE_RATE as f64) as i64;
        if frames_sent - elapsed_frames >= BUFFERED_LEAD_SAMPLES {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }

        if source_ended {
            break;
        }

        let mut samples = [0i16; AAC_FRAMES_PER_PACKET * 2];
        let frames = source.fill(&mut samples);
        if frames == 0 {
            source_ended = true;
            continue;
        }
        if frames < AAC_FRAMES_PER_PACKET {
            // Zero-pad the final partial block.
            for v in &mut samples[frames * 2..] {
                *v = 0;
            }
        }

        let aac_frame = encoder.encode(&samples)?;
        if aac_frame.is_empty() {
            // Encoder still priming: no output yet, don't advance rtptime.
            continue;
        }

        let block = build_buffered_audio_block(&mut cipher, seq, rtptime, AAC_44100_F24_2_SSRC, &aac_frame);
        data_stream.write_all(&block)?;

        seq = seq.wrapping_add(1);
        rtptime = rtptime.wrapping_add(AAC_FRAMES_PER_PACKET as u32);
        frames_sent += AAC_FRAMES_PER_PACKET as i64;

        if last_feedback.elapsed() >= Duration::from_secs(2) {
            if let Err(e) = session.feedback() {
                warn!("feedback failed: {e}");
            }
            last_feedback = Instant::now();
        }
    }

    info!("stream finished, tearing down");
    session.set_rate(0).ok();
    session.teardown()?;
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
