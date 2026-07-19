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

mod pairings;
mod source;
pub use pairings::PairingStore;
pub use source::{CaptureSource, SineSource, WavSource};

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
fn open_event_channel(peer_ip: std::net::IpAddr, event_port: u16) -> Option<TcpStream> {
    match TcpStream::connect(SocketAddr::new(peer_ip, event_port)) {
        Ok(s) => {
            s.set_nodelay(true).ok();
            if let Ok(mut rdr) = s.try_clone() {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 2048];
                    while let Ok(n) = std::io::Read::read(&mut rdr, &mut buf) {
                        if n == 0 {
                            break;
                        }
                    }
                });
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
    let _event = open_event_channel(peer_ip, ports.event_port);

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
        &[(addr, device_id.to_string())],
        source,
        volume_db,
        latency_ms,
    )
}

/// One receiver's live state inside a (possibly multi-room) buffered stream.
struct BufferedReceiver {
    name: String,
    session: StreamSession,
    cipher: AudioCipher,
    /// Bounded queue to this receiver's TCP writer thread. `None` once closed.
    tx: Option<std::sync::mpsc::SyncSender<Vec<u8>>>,
    writer: Option<std::thread::JoinHandle<()>>,
    /// Keeps the reverse event channel open for the session lifetime.
    _event: Option<TcpStream>,
    /// Keeps the (idle) control socket bound for the session lifetime.
    _control: ControlChannel,
    alive: bool,
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

/// Multi-room buffered streaming: the same AAC audio, time-synchronized, to
/// every receiver in `receivers` (`(addr, device_id)` pairs).
///
/// How the group stays in sync: one PTP node serves the whole timing group
/// (SETPEERS tells every receiver about all the others, so group-wide BMCA
/// elects a single grandmaster — ours for Shairport-only groups, an Apple
/// receiver's own clock when one is present), and every session gets an
/// IDENTICAL SETRATEANCHORTIME (same timeline, same network time, same
/// rtpTime). Each receiver then plays frame N at the same group-clock
/// instant. Audio is encoded once and encrypted per-receiver (each SETUP
/// negotiates its own AEAD key); per-receiver writer threads with bounded
/// queues isolate a stalling receiver from the rest of the group.
pub fn stream_audio_buffered_multi(
    receivers: &[(SocketAddr, String)],
    source: &mut dyn AudioSource,
    volume_db: Option<f32>,
    latency_ms: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    if receivers.is_empty() {
        return Err("no receivers given".into());
    }
    let group_ips: Vec<std::net::IpAddr> = receivers.iter().map(|(a, _)| a.ip()).collect();

    // One PTP node for the whole group, running before any receiver starts
    // monitoring us (and observing their masters before we anchor).
    let ptp = PtpMaster::start_multi(&group_ips)?;

    // --- Per-receiver RTSP negotiation ---
    let mut group: Vec<BufferedReceiver> = Vec::new();
    for (addr, device_id) in receivers {
        let name = format!("{addr}");
        let setup = (|| -> Result<BufferedReceiver, Box<dyn std::error::Error>> {
            let control = ControlChannel::bind()?;
            let mut session = connect_session(*addr, device_id)?;
            let peer_ip = session.peer_ip();
            session.setup_timing(TimingConfig::Ptp)?;
            session.setup_stream(StreamFormat::AacBuffered, control.port)?;
            let event = open_event_channel(peer_ip, session.ports.event_port);
            if let Err(e) = session.set_peers() {
                warn!("SETPEERS failed (continuing): {e}");
            }
            let cipher = AudioCipher::new(&session.shk);
            Ok(BufferedReceiver {
                name: name.clone(),
                session,
                cipher,
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
                TcpStream::connect(SocketAddr::new(peer_ip, r.session.ports.data_port))?;
            data_stream.set_nodelay(true).ok();
            r.session.record(seq as u16, first_rtptime)?;

            // ~256 blocks ≈ 6 s of audio: enough to absorb TCP hiccups,
            // small enough to bound memory and detect a truly dead peer.
            let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<u8>>(256);
            let mut stream = data_stream;
            let name = r.name.clone();
            r.writer = Some(std::thread::spawn(move || {
                for block in rx {
                    if let Err(e) = stream.write_all(&block) {
                        warn!(receiver = %name, "data write failed: {e}");
                        break; // dropping rx signals the main loop
                    }
                }
            }));
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

    // --- Anchors: ONE shared physical instant (rtpTime=0 plays latency_ms
    // from now), expressed per receiver on the timeline that receiver
    // actually follows — ours for Shairport-style receivers, its own
    // grandmaster for Apple receivers (translated by the measured offset).
    // Same instant on every clock = synchronized rooms, without relying on
    // receivers being able to see each other's clocks.
    let t_local = ptp_now_ns() + latency_ms * 1_000_000;
    for r in &mut group {
        let tl = ptp.timeline_for(r.session.peer_ip());
        info!(
            receiver = %r.name,
            gm = format!("{:016x}", tl.gm_id),
            offset_ms = tl.offset_ns as f64 / 1e6,
            foreign = tl.gm_id != ptp.clock_id,
            "anchor timeline"
        );
        let anchor_ns = t_local.wrapping_add_signed(tl.offset_ns);
        let (anchor_secs, anchor_frac) = ptp_ns_to_secs_frac(anchor_ns);
        if let Err(e) =
            r.session
                .set_rate_anchor(tl.gm_id, first_rtptime, anchor_secs, anchor_frac, 1)
        {
            warn!(receiver = %r.name, "anchor failed — dropping: {e}");
            r.alive = false;
        }
        if let Some(db) = volume_db {
            if let Err(e) = r.session.set_volume(db) {
                warn!(receiver = %r.name, "set_volume failed (continuing): {e}");
            }
        }
    }
    group.retain(|r| r.alive);
    if group.is_empty() {
        return Err("no receiver accepted the anchor".into());
    }

    // --- Encode once + fan out (send-ahead pacing) ---
    let mut encoder = AacEncoder::new()?;
    let start_instant = Instant::now();
    let mut last_feedback = Instant::now();

    info!(receivers = group.len(), "streaming buffered AAC audio");

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
        if !group.iter().any(|r| r.alive) {
            warn!("all receivers dropped — stopping stream");
            break;
        }

        seq = seq.wrapping_add(1);
        rtptime = rtptime.wrapping_add(AAC_FRAMES_PER_PACKET as u32);
        frames_sent += AAC_FRAMES_PER_PACKET as i64;

        if last_feedback.elapsed() >= Duration::from_secs(2) {
            for r in &mut group {
                if r.alive {
                    if let Err(e) = r.session.feedback() {
                        warn!(receiver = %r.name, "feedback failed: {e}");
                    }
                }
            }
            last_feedback = Instant::now();
        }
    }

    // Wait for the queued audio to actually PLAY OUT before tearing down.
    // rtpTime=0 starts at anchor time (latency_ms after the anchor was
    // computed), and the send-ahead pacing keeps us up to the whole lead
    // window ahead of wall clock — tearing down immediately makes receivers
    // dump the unplayed tail (for a short source: ALL of it, silently).
    let played = Duration::from_secs_f64(frames_sent as f64 / SAMPLE_RATE as f64)
        + Duration::from_millis(latency_ms + 250);
    let elapsed = start_instant.elapsed();
    if played > elapsed {
        let wait = played - elapsed;
        info!(wait_ms = wait.as_millis() as u64, "draining playout before teardown");
        std::thread::sleep(wait);
    }

    info!("stream finished, tearing down");
    for r in &mut group {
        r.finish();
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
