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

/// Peak |sample| below which a packet counts as silence, for live-capture
/// pause detection (~ -54 dBFS). Real system playback sits far above this;
/// a paused source is exact zeros (WASAPI loopback stops delivering, so the
/// capture source pads zeros).
const SILENCE_PEAK: u16 = 64;
/// How long a live source must stay silent before the AirPlay stream is
/// paused (`rate=0`). Auto-resumes (re-anchor) the instant audio returns.
const PAUSE_AFTER_SILENCE: Duration = Duration::from_millis(1500);

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
    session: StreamSession,
    cipher: AudioCipher,
    /// Per-receiver anchor offset in ns (from `GroupTarget::offset_ms`).
    offset_ns: i64,
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
pub fn stream_audio_buffered_multi(
    targets: &[GroupTarget],
    source: &mut dyn AudioSource,
    volume_db: Option<f32>,
    latency_ms: u64,
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
            let event = open_event_channel(peer_ip, session.ports.event_port);
            if let Err(e) = session.set_peers() {
                warn!("SETPEERS failed (continuing): {e}");
            }
            let cipher = AudioCipher::new(&session.shk);
            Ok(BufferedReceiver {
                name: name.clone(),
                session,
                cipher,
                offset_ns: target.offset_ms * 1_000_000,
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
    // actually follows plus its user offset. Same instant on every clock =
    // synchronized rooms, without relying on receivers seeing each other's
    // clocks.
    let t_local = ptp_now_ns() + latency_ms * 1_000_000;
    for r in &mut group {
        if let Err(e) = anchor_receiver(&ptp, r, t_local, first_rtptime) {
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

    // --- Encode once + fan out (send-ahead pacing, with pause/resume) ---
    let live = source.is_live();
    let mut encoder = AacEncoder::new()?;
    // `pace_origin`/`frames_sent` are the wall-clock pacing baseline; both
    // reset on every resume so post-pause playback re-paces cleanly.
    let mut pace_origin = Instant::now();
    let mut frames_sent: i64 = 0;
    let mut last_feedback = Instant::now();

    info!(receivers = group.len(), live, "streaming buffered AAC audio");

    let mut rtptime: u32 = first_rtptime;
    let mut paused = false;
    let mut silent_since: Option<Instant> = None;

    loop {
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
                    let t_local = ptp_now_ns() + latency_ms * 1_000_000;
                    for r in &mut group {
                        if r.alive {
                            if let Err(e) = anchor_receiver(&ptp, r, t_local, rtptime) {
                                warn!(receiver = %r.name, "resume anchor failed — dropping: {e}");
                                r.alive = false;
                            }
                        }
                    }
                    group.retain(|r| r.alive);
                    if group.is_empty() {
                        warn!("all receivers dropped on resume — stopping");
                        break;
                    }
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

        service_feedback(&mut group, &mut last_feedback);
    }

    // Wait for the queued audio to actually PLAY OUT before tearing down.
    // rtpTime advances with the send-ahead window, up to the whole lead ahead
    // of wall clock — tearing down immediately makes receivers dump the
    // unplayed tail (for a short source: ALL of it, silently). If we ended
    // while paused there is nothing buffered, so this naturally waits ~0.
    let played = Duration::from_secs_f64(frames_sent as f64 / SAMPLE_RATE as f64)
        + Duration::from_millis(latency_ms + 250);
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
