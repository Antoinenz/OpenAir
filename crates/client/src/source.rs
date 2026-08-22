//! [`AudioSource`] implementations: a synthetic sine tone (hardware smoke
//! test), a WAV file reader, and a live system-audio capture source — all
//! resampled/format-converted to the pipeline's fixed format (interleaved
//! stereo i16 @ 44100 Hz).
use std::collections::VecDeque;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hound::{SampleFormat, WavReader};
use tracing::{debug, warn};

use crate::resample::Resampler;
use crate::{AudioSource, SAMPLE_RATE};

/// Generates a sine tone at `freq` Hz for a fixed duration. Reproduces the
/// exact sample generation of the original `stream_tone` loop: 0.6 amplitude,
/// same value written to both channels.
pub struct SineSource {
    phase: f32,
    step: f32,
    frames_left: u64,
}

impl SineSource {
    pub fn new(freq: f32, seconds: u32) -> Self {
        let step = 2.0 * std::f32::consts::PI * freq / SAMPLE_RATE as f32;
        let frames_left = u64::from(seconds) * u64::from(SAMPLE_RATE);
        SineSource {
            phase: 0.0,
            step,
            frames_left,
        }
    }
}

impl AudioSource for SineSource {
    fn fill(&mut self, buf: &mut [i16]) -> usize {
        let max_frames = buf.len() / 2;
        let frames = max_frames.min(self.frames_left as usize);
        for frame in buf[..frames * 2].chunks_exact_mut(2) {
            let v = (self.phase.sin() * 0.6 * f32::from(i16::MAX)) as i16;
            frame[0] = v;
            frame[1] = v;
            self.phase += self.step;
        }
        self.frames_left -= frames as u64;
        frames
    }
}


/// Reads a WAV file and yields interleaved stereo i16 samples at 44100 Hz,
/// regardless of the file's native format.
///
/// Supported inputs: 16-bit integer PCM or 32-bit float, 1 or 2 channels,
/// any sample rate. Mono is duplicated to both channels; float samples are
/// scaled by `i16::MAX` with clamping; sample rates other than 44100 Hz go
/// through [`crate::resample::Resampler`].
///
/// Decoding happens incrementally in [`fill`](AudioSource::fill): the
/// decoder keeps a small internal buffer of source-rate stereo frames rather
/// than loading the whole file into memory.
pub struct WavSource {
    reader: WavReader<BufReader<File>>,
    src_channels: u16,
    sample_format: SampleFormat,
    resampler: Resampler,
}

impl WavSource {
    pub fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let reader = WavReader::open(path)?;
        let spec = reader.spec();

        if spec.channels != 1 && spec.channels != 2 {
            return Err(format!(
                "unsupported channel count: {} (only mono/stereo supported)",
                spec.channels
            )
            .into());
        }
        match (spec.sample_format, spec.bits_per_sample) {
            (SampleFormat::Int, 16) => {}
            (SampleFormat::Float, 32) => {}
            (fmt, bits) => {
                return Err(format!(
                    "unsupported sample format: {:?} {}-bit (only 16-bit int or 32-bit float supported)",
                    fmt, bits
                )
                .into())
            }
        }

        let resampler = Resampler::new(spec.sample_rate);

        Ok(WavSource {
            reader,
            src_channels: spec.channels,
            sample_format: spec.sample_format,
            resampler,
        })
    }
}

/// Reads one frame (1 or 2 source samples) and returns it as a stereo i16
/// pair, duplicating mono to both channels and scaling float to i16 range.
fn read_stereo_frame(
    reader: &mut WavReader<BufReader<File>>,
    channels: u16,
    format: SampleFormat,
) -> Option<[i16; 2]> {
    let to_i16 = |v: f64| -> i16 {
        v.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
    };

    match format {
        SampleFormat::Int => {
            let mut samples = reader.samples::<i32>();
            let l = samples.next()?.ok()? as i16;
            if channels == 1 {
                Some([l, l])
            } else {
                let r = samples.next()?.ok()? as i16;
                Some([l, r])
            }
        }
        SampleFormat::Float => {
            let mut samples = reader.samples::<f32>();
            let l_f = samples.next()?.ok()?;
            let l = to_i16(f64::from(l_f) * f64::from(i16::MAX));
            if channels == 1 {
                Some([l, l])
            } else {
                let r_f = samples.next()?.ok()?;
                let r = to_i16(f64::from(r_f) * f64::from(i16::MAX));
                Some([l, r])
            }
        }
    }
}

impl AudioSource for WavSource {
    fn fill(&mut self, buf: &mut [i16]) -> usize {
        let reader = &mut self.reader;
        let src_channels = self.src_channels;
        let sample_format = self.sample_format;
        self.resampler
            .fill(buf, || read_stereo_frame(reader, src_channels, sample_format))
    }
}


/// Minimum amount of device-rate audio (in ms) buffered in the ring before
/// the first `fill()` call starts producing output. Absorbs startup jitter
/// from the capture callback so the very first packets aren't silence.
const PREBUFFER_MS: u32 = 200;
/// Ceiling on how long [`CaptureSource::fill`] will block waiting for the
/// prebuffer to fill, in ms. If the device never produces audio (e.g. no
/// active playback), give up and stream silence rather than hang forever.
const PREBUFFER_MAX_WAIT_MS: u32 = 500;
const PREBUFFER_POLL_MS: u64 = 5;
/// Ceiling on how long a single blocking-mode `fill()` waits for live ring
/// data before giving up and padding silence. Long enough to cover normal
/// capture-callback jitter (WASAPI delivers in ~10 ms chunks), short enough
/// that a paused source is noticed within a couple of packets.
const BLOCKING_WAIT_MS: u64 = 60;

/// Ring exceeding this many ms of buffered audio indicates the sender is
/// falling behind the device's capture rate; without drift trim, the ring is
/// drained back down to avoid unbounded latency growth.
///
/// **Discarding is the fallback, not the plan.** It is what caused a
/// three-room session to lose its buffer permanently: a 1.5 s network stall
/// left 1.5 s of audio in the ring — exactly the audio needed to catch back
/// up — and the very next `fill()` threw 1.2 s of it away as though it were
/// drift. Headroom went from 2000 ms to 800 ms and stayed there.
const DRIFT_HIGH_WATER_MS: u32 = 1000;
/// Target ms of buffered audio left after a drain.
const DRIFT_DRAIN_TARGET_MS: u32 = 300;

/// Ring level the drift trim holds, in ms.
///
/// Enough to absorb capture-callback jitter, small enough that it is not
/// itself a source of latency.
const RING_TARGET_MS: u32 = 250;

/// Error band inside which no trim is applied at all.
///
/// Without it the ratio would wander continuously on ordinary jitter. Inside
/// the band the resampler runs at exactly nominal.
const RING_DEADBAND_MS: u32 = 60;

/// Ring error producing full trim deflection.
const RING_FULL_SCALE_MS: u32 = 500;

/// Largest ratio deviation the trim will apply.
///
/// 0.5% moves the pitch about 8.6 cents, where a semitone is 100 — inaudible
/// on music unless you are listening for it. Correcting a 500 ms deficit at
/// this rate takes ~100 s, which is the trade: slow and unnoticeable, against
/// instant and obvious.
const MAX_DRIFT_TRIM: f64 = 0.005;

/// Beyond this the ring is drained regardless of trim.
///
/// A backstop for the case trim cannot fix in time — the capture ring is only
/// [`RING_CAPACITY_SECONDS`](openair_capture) long, and the callback drops the
/// oldest samples once it is full. Discarding deliberately here is better than
/// letting the producer do it arbitrarily.
const RING_PANIC_MS: u32 = 3000;

/// Live system-audio capture source: resamples from a shared ring buffer
/// (filled by `openair_capture::SystemCapture` on a cpal callback thread,
/// device-rate stereo i16) to the pipeline's fixed 44100 Hz format.
///
/// `SystemCapture` (and its `!Send` `cpal::Stream`) never enters this crate;
/// only the `Arc<Mutex<VecDeque<i16>>>` ring crosses the thread boundary.
pub struct CaptureSource {
    ring: Arc<Mutex<VecDeque<i16>>>,
    /// The rate the producer is currently capturing at.
    ///
    /// Shared rather than owned because `--handoff` can swap the capture
    /// device mid-stream, and the two devices need not run at the same rate.
    /// Read once per `fill()` into `device_rate`.
    rate_source: Arc<AtomicU32>,
    /// Cached copy of `rate_source`, so the several derived sizes below
    /// (prebuffer, drift high-water, drain target) stay plain arithmetic.
    device_rate: u32,
    /// Count of observed rate changes, for tests and diagnostics.
    rate_changes: u64,
    /// Close ring-level error by trimming the resample ratio rather than by
    /// discarding audio. See [`CaptureSource::apply_drift_control`].
    drift_trim: bool,
    resampler: Resampler,
    /// Total 44100 Hz output frames left to produce, if a max duration was
    /// requested. `None` means stream indefinitely.
    frames_remaining: Option<u64>,
    /// Set once the initial prebuffer wait has completed (or been skipped),
    /// so subsequent `fill()` calls don't re-wait.
    prebuffer_done: bool,
    /// When set and `true`, the next `fill()` call ends the stream (returns
    /// 0) regardless of `frames_remaining`. Lets callers (e.g. a Ctrl+C
    /// handler) stop an indefinite capture cleanly.
    stop: Option<Arc<AtomicBool>>,
    /// Diagnostics: fill() call counter and total silence-padded frames.
    fills: u64,
    silent_frames: u64,
    /// Blocking mode (for buffered/send-ahead pipelines): `fill()` waits for
    /// real ring data instead of padding silence, which rate-limits the
    /// send-ahead loop to the live capture rate. Without this, a buffered
    /// pipeline "racing ahead" of a live source pads its whole lead window
    /// with silence-mixed-with-dribbles — audibly glitchy for the first
    /// seconds of a session.
    blocking: bool,
}

impl CaptureSource {
    /// `ring`/`device_rate` come from `openair_capture::SystemCapture`.
    /// `max_seconds`, if set, bounds the total output to that many seconds
    /// of 44100 Hz audio; `fill` returns 0 (end of stream) once exhausted.
    /// `stop`, if set, is checked at the start of each `fill()`: once it's
    /// `true`, `fill()` returns 0 (end of stream) even if `max_seconds`
    /// hasn't elapsed (or was never set), so an indefinite capture can be
    /// stopped cleanly (e.g. via Ctrl+C).
    pub fn new(
        ring: Arc<Mutex<VecDeque<i16>>>,
        device_rate: u32,
        max_seconds: Option<u32>,
        stop: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self::new_with_rate(
            ring,
            Arc::new(AtomicU32::new(device_rate)),
            max_seconds,
            stop,
        )
    }

    /// As [`CaptureSource::new`], but the capture rate is shared and may change
    /// while the stream runs — which is what happens when `--handoff` switches
    /// the capture device to a virtual cable running at a different rate.
    pub fn new_with_rate(
        ring: Arc<Mutex<VecDeque<i16>>>,
        rate: Arc<AtomicU32>,
        max_seconds: Option<u32>,
        stop: Option<Arc<AtomicBool>>,
    ) -> Self {
        let frames_remaining = max_seconds.map(|s| u64::from(s) * u64::from(SAMPLE_RATE));
        let device_rate = rate.load(Ordering::Relaxed);
        let resampler = Resampler::new(device_rate);
        CaptureSource {
            ring,
            rate_source: rate,
            device_rate,
            rate_changes: 0,
            drift_trim: true,
            resampler,
            frames_remaining,
            prebuffer_done: false,
            stop,
            fills: 0,
            silent_frames: 0,
            blocking: false,
        }
    }

    /// How many times the capture rate has changed under this source.
    pub fn rate_changes(&self) -> u64 {
        self.rate_changes
    }

    /// Adopt a new capture rate if the producer has changed device.
    ///
    /// Compared against the cached value rather than applied unconditionally:
    /// re-priming on every `fill()` would put a discontinuity in every buffer.
    /// A zero is ignored — that is an uninitialised atomic, not a real rate.
    fn sync_rate(&mut self) {
        let current = self.rate_source.load(Ordering::Relaxed);
        if current == self.device_rate || current == 0 {
            return;
        }
        tracing::info!(
            from_hz = self.device_rate,
            to_hz = current,
            "capture device rate changed — following it"
        );
        self.device_rate = current;
        self.rate_changes += 1;
        self.resampler.set_rate(current);
    }

    /// Enable blocking mode: `fill()` waits (bounded) for live ring data
    /// instead of silence-padding. Use with send-ahead (buffered) pipelines;
    /// realtime pipelines must stay non-blocking so RTP pacing never stalls.
    pub fn with_blocking(mut self) -> Self {
        self.blocking = true;
        self
    }

    /// In blocking mode: wait (short polls, bounded) until the ring holds
    /// enough device-rate samples to produce `frames` output frames.
    ///
    /// The bound is short (`BLOCKING_WAIT_MS`): when live audio is flowing the
    /// data is there within one packet time so this returns promptly and
    /// rate-limits the send loop to real time; when the source has gone dry
    /// (playback paused — WASAPI loopback stops delivering) it must give up
    /// quickly and let the caller pad silence, so the pipeline's pause/resume
    /// state machine stays responsive instead of stalling ~1 s per fill.
    fn wait_for_frames(&self, frames: usize) {
        // Output frames → device-rate samples (stereo interleaved), plus one
        // spare frame for the resampler bracket.
        let needed =
            ((frames as f64 * f64::from(self.device_rate) / f64::from(SAMPLE_RATE)) as usize + 2)
                * 2;
        let deadline = Instant::now() + Duration::from_millis(BLOCKING_WAIT_MS);
        loop {
            if self.ring.lock().unwrap().len() >= needed || Instant::now() >= deadline {
                break;
            }
            if let Some(stop) = &self.stop {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(PREBUFFER_POLL_MS));
        }
    }

    /// Pulls one interleaved stereo frame from the front of the ring, if
    /// available.
    fn pull_ring_frame(ring: &Mutex<VecDeque<i16>>) -> Option<[i16; 2]> {
        let mut guard = ring.lock().unwrap();
        let l = guard.pop_front()?;
        // If only one sample is available the frame is torn (shouldn't
        // happen since the capture callback always pushes pairs); treat as
        // exhausted rather than panicking on the missing R sample.
        let r = guard.pop_front()?;
        Some([l, r])
    }

    /// Blocks (in short polling increments) until the ring holds at least
    /// `PREBUFFER_MS` of device-rate audio, or `PREBUFFER_MAX_WAIT_MS`
    /// elapses. A live capture needs a small startup cushion so the
    /// resampler isn't immediately starved by callback-thread jitter.
    fn wait_for_prebuffer(&mut self) {
        let target_samples =
            (self.device_rate as u64 * 2 * u64::from(PREBUFFER_MS) / 1000) as usize;
        let deadline = Instant::now() + Duration::from_millis(u64::from(PREBUFFER_MAX_WAIT_MS));
        loop {
            let len = self.ring.lock().unwrap().len();
            if len >= target_samples || Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(PREBUFFER_POLL_MS));
        }
        self.prebuffer_done = true;
    }

    /// Enable or disable closing ring-level error by ratio trim.
    ///
    /// Disabled falls back to discarding audio above the high-water mark,
    /// which is the older behaviour and audible when it fires.
    pub fn with_drift_trim(mut self, enabled: bool) -> Self {
        self.drift_trim = enabled;
        self
    }

    /// The trim currently applied to the resample ratio, for observers.
    pub fn drift_trim_ratio(&self) -> f64 {
        self.resampler.trim()
    }

    /// Hold the capture ring near [`RING_TARGET_MS`].
    ///
    /// The producer (a cpal callback on the device's clock) and the consumer
    /// (this source, paced by the RTP send loop) run on independent clocks, so
    /// the ring level wanders. A network stall makes it jump.
    ///
    /// Both are corrected the same way: nudge the resample ratio so slightly
    /// more or slightly less source audio is consumed per output frame, and
    /// let the error close over tens of seconds. Nothing is discarded and
    /// nothing is re-anchored — the audio is imperceptibly fast or slow while
    /// it happens.
    ///
    /// Falls back to discarding when trim is off, or when the source is at the
    /// pipeline rate and is therefore being copied bit-for-bit with no filter
    /// to retune.
    fn apply_drift_control(&mut self) {
        let level_ms = self.ring_ms();

        // Backstop first: past this the producer would start dropping the
        // oldest samples itself, and a deliberate drain beats an arbitrary one.
        if level_ms > RING_PANIC_MS {
            self.drain_ring_to(RING_TARGET_MS);
            warn!(
                level_ms,
                "capture ring far beyond what trim can absorb — draining"
            );
            return;
        }

        if !self.drift_trim || !self.resampler.can_trim() {
            if level_ms > DRIFT_HIGH_WATER_MS {
                debug!(level_ms, "capture ring overfull, draining for drift");
                self.drain_ring_to(DRIFT_DRAIN_TARGET_MS);
            }
            return;
        }

        let target = self.resampler_trim_for(level_ms);
        if (target - self.resampler.trim()).abs() > f64::EPSILON {
            debug!(
                level_ms,
                trim = target,
                "adjusting resample ratio to close ring drift"
            );
            self.resampler.set_trim(target);
        }
    }

    /// The trim that should be applied at a given ring level.
    ///
    /// A ring above target means audio is piling up, so *more* of it should be
    /// consumed per output frame — which is a ratio below nominal. Below
    /// target is the reverse. Quantised so ordinary jitter does not re-ramp
    /// the resampler on every packet.
    fn resampler_trim_for(&self, level_ms: u32) -> f64 {
        let error = f64::from(level_ms) - f64::from(RING_TARGET_MS);
        if error.abs() <= f64::from(RING_DEADBAND_MS) {
            return 1.0;
        }
        let norm = (error / f64::from(RING_FULL_SCALE_MS)).clamp(-1.0, 1.0);
        let trim = 1.0 - norm * MAX_DRIFT_TRIM;
        (trim * 10_000.0).round() / 10_000.0
    }

    /// Buffered audio in the ring, in ms.
    fn ring_ms(&self) -> u32 {
        let samples = self.ring.lock().map(|g| g.len()).unwrap_or(0) as u64;
        let per_ms = u64::from(self.device_rate) * 2 / 1000;
        if per_ms == 0 {
            return 0;
        }
        (samples / per_ms) as u32
    }

    fn drain_ring_to(&self, target_ms: u32) {
        let keep = (u64::from(self.device_rate) * 2 * u64::from(target_ms) / 1000) as usize;
        if let Ok(mut guard) = self.ring.lock() {
            if guard.len() > keep {
                let drain = guard.len() - keep;
                guard.drain(..drain);
            }
        }
    }
}

impl AudioSource for CaptureSource {
    fn is_live(&self) -> bool {
        true
    }

    fn fill(&mut self, buf: &mut [i16]) -> usize {
        // Before anything reads `device_rate` — the prebuffer size and the
        // drift thresholds below are all derived from it.
        self.sync_rate();
        if let Some(stop) = &self.stop {
            if stop.load(Ordering::Relaxed) {
                return 0;
            }
        }

        let mut max_frames = buf.len() / 2;

        if let Some(remaining) = self.frames_remaining {
            if remaining == 0 {
                return 0;
            }
            // Cap this call's output at what's left of the requested
            // duration, mirroring SineSource: the pipeline zero-pads a
            // final partial packet itself, so we must not report more
            // frames than the duration budget allows.
            max_frames = max_frames.min(remaining as usize);
        }

        if !self.prebuffer_done {
            if self.blocking {
                // Live low-latency start: everything captured while the
                // session was being negotiated is stale — drop all but the
                // newest ~100 ms so playback starts near "now" instead of
                // seconds in the past.
                let keep = (self.device_rate as usize / 10) * 2;
                let mut guard = self.ring.lock().unwrap();
                let len = guard.len();
                if len > keep {
                    guard.drain(..len - keep);
                }
                drop(guard);
            }
            self.wait_for_prebuffer();
        }

        if self.blocking {
            self.wait_for_frames(max_frames);
        }

        self.apply_drift_control();

        let ring = &self.ring;
        let written = self
            .resampler
            .fill(&mut buf[..max_frames * 2], || Self::pull_ring_frame(ring));

        // A live capture must never starve the RTP pacing loop: if the ring
        // ran dry mid-fill (written < requested), pad the remainder with
        // silence and still report the full requested frame count. This
        // also means the resampler's `next` bracket is now `None`
        // (exhausted); refill it with silence so future calls keep working
        // once real audio resumes being available in `pull_ring_frame`.
        let mut total = written;
        if written < max_frames {
            for v in &mut buf[written * 2..max_frames * 2] {
                *v = 0;
            }
            total = max_frames;
            if self.resampler.is_exhausted() {
                // Re-arm rather than rebuild: a rebuild would throw away the
                // filter's history and put a discontinuity at every moment the
                // ring briefly ran dry, which on a live capture is often.
                self.resampler.rearm();
            }
        }

        if let Some(remaining) = &mut self.frames_remaining {
            *remaining = remaining.saturating_sub(total as u64);
        }

        // Periodic capture health log: how much live audio is buffered and
        // how much of this fill was real vs. silence padding.
        self.fills += 1;
        self.silent_frames += (total - written) as u64;
        if self.fills.is_multiple_of(250) {
            let ring_len = self.ring.lock().map(|g| g.len()).unwrap_or(0);
            tracing::debug!(
                ring_frames = ring_len / 2,
                silence_padded_frames = self.silent_frames,
                "capture health"
            );
        }

        total
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hound::{WavSpec, WavWriter};
    use std::f32::consts::PI;

    #[test]
    fn sine_source_fills_exact_frame_count() {
        let mut src = SineSource::new(440.0, 1);
        let mut buf = [0i16; 352 * 2];
        let frames = src.fill(&mut buf);
        assert_eq!(frames, 352);
    }

    #[test]
    fn sine_source_is_continuous_across_calls() {
        // Max slope of a 0.6-amplitude 440Hz sine at 44.1kHz sampled per-frame:
        // d/dn [0.6*I16MAX*sin(step*n)] has max |delta| ~= 0.6*I16MAX*step.
        let freq = 440.0f32;
        let step = 2.0 * PI * freq / 44100.0;
        let max_slope = 0.6 * f32::from(i16::MAX) * step;

        let mut src = SineSource::new(freq, 2);
        let mut buf1 = [0i16; 352 * 2];
        let mut buf2 = [0i16; 352 * 2];
        assert_eq!(src.fill(&mut buf1), 352);
        assert_eq!(src.fill(&mut buf2), 352);

        // Compare last sample of buf1 to first sample of buf2 (left channel).
        let last = buf1[buf1.len() - 2] as f32;
        let first = buf2[0] as f32;
        let delta = (first - last).abs();
        assert!(
            delta <= max_slope * 1.5,
            "phase discontinuity across fill() calls: delta={delta}, max_slope={max_slope}"
        );
    }

    #[test]
    fn sine_source_ends_at_zero() {
        // 1 second @ 44100 Hz = 44100 frames total; drain in 352-frame
        // packets until exhausted, then confirm fill() reports 0.
        let mut src = SineSource::new(440.0, 1);
        let mut buf = [1i16; 352 * 2];
        let mut total = 0u64;
        loop {
            let frames = src.fill(&mut buf);
            if frames == 0 {
                break;
            }
            total += frames as u64;
        }
        assert_eq!(total, 44100);
        // Further calls keep reporting 0.
        assert_eq!(src.fill(&mut buf), 0);
    }

    fn write_test_wav(
        path: &std::path::Path,
        sample_rate: u32,
        channels: u16,
        format: SampleFormat,
        bits: u16,
        num_frames: usize,
    ) {
        let spec = WavSpec {
            channels,
            sample_rate,
            bits_per_sample: bits,
            sample_format: format,
        };
        let mut writer = WavWriter::create(path, spec).unwrap();
        for n in 0..num_frames {
            let t = n as f32 / sample_rate as f32;
            let v = (2.0 * PI * 220.0 * t).sin();
            match format {
                SampleFormat::Int => {
                    let sample = (v * 0.5 * i16::MAX as f32) as i16;
                    for _ in 0..channels {
                        writer.write_sample(sample as i32).unwrap();
                    }
                }
                SampleFormat::Float => {
                    let sample = v * 0.5;
                    for _ in 0..channels {
                        writer.write_sample(sample).unwrap();
                    }
                }
            }
        }
        writer.finalize().unwrap();
    }

    fn drain_all(src: &mut WavSource) -> Vec<i16> {
        let mut out = Vec::new();
        loop {
            let mut buf = [0i16; 352 * 2];
            let frames = src.fill(&mut buf);
            if frames == 0 {
                break;
            }
            out.extend_from_slice(&buf[..frames * 2]);
        }
        out
    }

    #[test]
    fn wav_source_16bit_stereo_44100() {
        let dir = std::env::temp_dir();
        let path = dir.join("openair_test_16_stereo_44100.wav");
        let input_frames = 4410; // 0.1s
        write_test_wav(&path, 44100, 2, SampleFormat::Int, 16, input_frames);

        let mut src = WavSource::open(&path).unwrap();
        let samples = drain_all(&mut src);
        let frames = samples.len() / 2;

        // Same rate, no resampling: frame count should match input closely.
        assert!(
            (frames as i64 - input_frames as i64).abs() <= 2,
            "frames={frames}, expected ~{input_frames}"
        );
        for s in &samples {
            assert!(s.is_finite_i16());
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wav_source_16bit_mono_22050_duplicates_channels() {
        let dir = std::env::temp_dir();
        let path = dir.join("openair_test_16_mono_22050.wav");
        let input_frames = 2205; // 0.1s at 22050Hz
        write_test_wav(&path, 22050, 1, SampleFormat::Int, 16, input_frames);

        let mut src = WavSource::open(&path).unwrap();
        let samples = drain_all(&mut src);
        let frames = samples.len() / 2;

        // Resampled from 22050 -> 44100: ~2x frames.
        let expected = (input_frames as f64 * 44100.0 / 22050.0) as i64;
        assert!(
            (frames as i64 - expected).abs() <= 2,
            "frames={frames}, expected ~{expected}"
        );

        // Mono duplication: every frame's L and R channel should be equal
        // (source L==R by construction since we wrote the mono value to
        // both channels' worth... actually mono has only 1 channel, so
        // after duplication L must equal R exactly for every output frame).
        for pair in samples.chunks_exact(2) {
            assert_eq!(pair[0], pair[1], "mono channel duplication mismatch");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn wav_source_f32_stereo_48000_resamples_and_is_finite() {
        let dir = std::env::temp_dir();
        let path = dir.join("openair_test_f32_stereo_48000.wav");
        let input_frames = 4800; // 0.1s at 48000Hz
        write_test_wav(&path, 48000, 2, SampleFormat::Float, 32, input_frames);

        let mut src = WavSource::open(&path).unwrap();
        let samples = drain_all(&mut src);
        let frames = samples.len() / 2;

        let expected = (input_frames as f64 * 44100.0 / 48000.0) as i64;
        assert!(
            (frames as i64 - expected).abs() <= 2,
            "frames={frames}, expected ~{expected}"
        );
        for s in &samples {
            assert!(s.is_finite_i16());
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Trivial helper trait so the "all samples finite/reasonable" assertion
    /// reads naturally; i16 is always finite, so this checks range sanity.
    trait FiniteI16 {
        fn is_finite_i16(&self) -> bool;
    }
    impl FiniteI16 for i16 {
        // i16 is always within its own range; the comparison is intentionally a
        // tautology (a readability stand-in for the float `is_finite` check).
        #[allow(clippy::absurd_extreme_comparisons)]
        fn is_finite_i16(&self) -> bool {
            *self >= i16::MIN && *self <= i16::MAX
        }
    }

    /// A rising ramp, so a test can tell one frame from another.
    fn ramp_frames(n: usize) -> Vec<[i16; 2]> {
        (0..n)
            .map(|i| [(2 * i) as i16, (2 * i + 1) as i16])
            .collect()
    }

    // -- CaptureSource ------------------------------------------------------

    /// Builds a `CaptureSource` with the prebuffer wait already satisfied
    /// (so tests never sleep) and preloads `ring` with `frames` device-rate
    /// stereo frames.
    fn capture_source_with_ring(
        device_rate: u32,
        max_seconds: Option<u32>,
        frames: &[[i16; 2]],
    ) -> (CaptureSource, Arc<Mutex<VecDeque<i16>>>) {
        let ring = Arc::new(Mutex::new(VecDeque::new()));
        {
            let mut guard = ring.lock().unwrap();
            for f in frames {
                guard.push_back(f[0]);
                guard.push_back(f[1]);
            }
        }
        let mut src = CaptureSource::new(ring.clone(), device_rate, max_seconds, None);
        // Skip the real prebuffer wait (which polls in 5ms increments up to
        // 500ms) — tests preload the ring directly, so there's nothing to
        // wait for.
        src.prebuffer_done = true;
        (src, ring)
    }

    /// As `capture_source_with_ring`, but the rate is shared so a test can
    /// change it mid-stream the way a device swap does.
    fn capture_source_with_shared_rate(
        rate: Arc<AtomicU32>,
        frames: &[[i16; 2]],
    ) -> (CaptureSource, Arc<Mutex<VecDeque<i16>>>) {
        let ring = Arc::new(Mutex::new(VecDeque::new()));
        {
            let mut guard = ring.lock().unwrap();
            for f in frames {
                guard.push_back(f[0]);
                guard.push_back(f[1]);
            }
        }
        let mut src = CaptureSource::new_with_rate(ring.clone(), rate, None, None);
        src.prebuffer_done = true;
        (src, ring)
    }

    #[test]
    fn a_rate_change_is_picked_up_mid_stream() {
        // The --handoff hazard: the producer is swapped to a device running at
        // a different rate. A source that keeps resampling at the old ratio
        // does not glitch -- it shifts pitch, which is easy to misdiagnose as
        // a receiver fault. So the ratio must follow the atomic.
        let rate = Arc::new(AtomicU32::new(44_100));
        let frames = ramp_frames(40_000);
        let (mut src, ring) = capture_source_with_shared_rate(Arc::clone(&rate), &frames);

        let mut buf = vec![0i16; 2_000];

        let before = ring.lock().unwrap().len();
        assert!(src.fill(&mut buf) > 0, "produced nothing at 1:1");
        let consumed_slow = before - ring.lock().unwrap().len();

        // Double the source rate: two source frames now collapse into one
        // output frame, so the same buffer consumes about twice as much ring.
        rate.store(88_200, Ordering::Relaxed);
        let before = ring.lock().unwrap().len();
        src.fill(&mut buf);
        let consumed_fast = before - ring.lock().unwrap().len();

        assert_eq!(src.rate_changes(), 1, "the change was observed exactly once");
        assert!(
            consumed_fast > consumed_slow * 3 / 2,
            "the resample ratio did not follow the rate:              {consumed_fast} consumed at 88200 vs {consumed_slow} at 44100"
        );
    }

    #[test]
    fn an_unchanged_rate_does_not_reset_the_resampler() {
        // Reading the atomic every fill() must not be mistaken for a change,
        // which would re-prime the interpolation bracket on every call and put
        // a discontinuity in every buffer.
        let rate = Arc::new(AtomicU32::new(48_000));
        let frames = ramp_frames(40_000);
        let (mut src, _ring) = capture_source_with_shared_rate(rate, &frames);

        let mut buf = vec![0i16; 512];
        for _ in 0..10 {
            src.fill(&mut buf);
        }
        assert_eq!(src.rate_changes(), 0, "no change was made, none should be seen");
    }

    #[test]
    fn capture_source_correct_output_frame_count_identity_rate() {
        let frames = ramp_frames(1000);
        let (mut src, _ring) = capture_source_with_ring(44100, None, &frames);

        let mut buf = [0i16; 352 * 2];
        let written = src.fill(&mut buf);
        assert_eq!(written, 352);
    }

    #[test]
    fn capture_source_pads_silence_when_ring_runs_dry() {
        // Only enough frames for a partial packet; fill() must still report
        // the full requested frame count, padded with silence.
        let frames = ramp_frames(100);
        let (mut src, _ring) = capture_source_with_ring(44100, None, &frames);

        let mut buf = [1i16; 352 * 2]; // sentinel value, not zero
        let written = src.fill(&mut buf);
        assert_eq!(written, 352, "must return full requested frame count");

        // Somewhere past the real 100 source frames, the tail must be
        // silence (zeros), since the ring ran dry.
        let tail = &buf[(352 - 50) * 2..];
        assert!(
            tail.iter().all(|&v| v == 0),
            "expected silence padding at tail once ring ran dry"
        );
    }

    #[test]
    fn capture_source_duration_limit_returns_zero_after_n_frames() {
        // 1 second cap at 44100 Hz output; ring has plenty of source data.
        let frames = ramp_frames(50_000);
        let (mut src, _ring) = capture_source_with_ring(44100, Some(1), &frames);

        let mut total = 0u64;
        let mut buf = [0i16; 352 * 2];
        loop {
            let written = src.fill(&mut buf);
            if written == 0 {
                break;
            }
            total += written as u64;
            // Safety valve against infinite loop if the duration limit is
            // broken.
            assert!(total <= 44100 * 2, "duration limit did not stop the stream");
        }
        assert_eq!(total, 44100);
        assert_eq!(src.fill(&mut buf), 0, "must keep returning 0 after limit");
    }

    #[test]
    fn capture_source_stop_flag_ends_stream() {
        // Preloaded ring with plenty of data and no duration limit; setting
        // the stop flag before fill() must still end the stream (return 0).
        let frames = ramp_frames(1000);
        let ring = Arc::new(Mutex::new(VecDeque::new()));
        {
            let mut guard = ring.lock().unwrap();
            for f in &frames {
                guard.push_back(f[0]);
                guard.push_back(f[1]);
            }
        }
        let stop = Arc::new(AtomicBool::new(true));
        let mut src = CaptureSource::new(ring, 44100, None, Some(stop));
        src.prebuffer_done = true;

        let mut buf = [1i16; 352 * 2];
        assert_eq!(src.fill(&mut buf), 0, "stop flag set before fill must end the stream");
    }

    #[test]
    fn capture_source_drift_guard_drains_overfull_ring() {
        // Preload far more than the 1s high-water mark at 44100 Hz
        // (2 * 44100 samples/sec of stereo i16 = 88200 samples/sec).
        let device_rate = 44100u32;
        let overfull_seconds = 2u32;
        let frame_count = device_rate as usize * overfull_seconds as usize;
        let frames = ramp_frames(frame_count);
        let (mut src, ring) = capture_source_with_ring(device_rate, None, &frames);

        // Sanity: ring holds ~2s of stereo audio before the guard runs.
        let before = ring.lock().unwrap().len();
        assert_eq!(before, frame_count * 2);

        src.apply_drift_control();

        let after = ring.lock().unwrap().len();
        let target_samples = (device_rate as u64 * 2 * u64::from(DRIFT_DRAIN_TARGET_MS) / 1000) as usize;
        assert_eq!(
            after, target_samples,
            "drift guard should drain ring down to the target watermark"
        );
    }

    #[test]
    fn a_backlogged_ring_is_trimmed_away_not_thrown_away() {
        // The bug this whole mechanism exists for. A network stall leaves the
        // ring holding exactly the audio needed to catch back up; the old
        // guard discarded it on the very next fill(), so the buffer headroom
        // never recovered. At a resampled rate the ring must survive intact.
        let device_rate = 48_000u32;
        // 1.5 s: past the old high-water mark, so the old guard would have
        // discarded most of it.
        let frames = ramp_frames(device_rate as usize * 3 / 2);
        let (mut src, ring) = capture_source_with_ring(device_rate, None, &frames);

        let before = ring.lock().unwrap().len();
        src.apply_drift_control();
        let after = ring.lock().unwrap().len();

        assert_eq!(after, before, "nothing may be discarded while trim can act");
        assert!(
            src.drift_trim_ratio() < 1.0,
            "an overfull ring should trim below nominal to consume faster, got {}",
            src.drift_trim_ratio()
        );
    }

    #[test]
    fn trim_disabled_falls_back_to_discarding() {
        let device_rate = 48_000u32;
        // Comfortably past the high-water mark rather than exactly on it.
        let frames = ramp_frames(device_rate as usize * 3 / 2);
        let (src, ring) = capture_source_with_ring(device_rate, None, &frames);
        let mut src = src.with_drift_trim(false);

        src.apply_drift_control();
        let after = ring.lock().unwrap().len();
        let target = (device_rate as u64 * 2 * u64::from(DRIFT_DRAIN_TARGET_MS) / 1000) as usize;
        assert_eq!(after, target, "the older behaviour is still available");
    }

    #[test]
    fn an_extreme_backlog_is_drained_even_with_trim_on() {
        // Trim cannot close several seconds before the capture callback starts
        // dropping the oldest samples itself. A deliberate drain beats an
        // arbitrary one.
        let device_rate = 48_000u32;
        let frames = ramp_frames(device_rate as usize * 4);
        let (mut src, ring) = capture_source_with_ring(device_rate, None, &frames);

        src.apply_drift_control();
        let after = ring.lock().unwrap().len();
        let target = (device_rate as u64 * 2 * u64::from(RING_TARGET_MS) / 1000) as usize;
        assert_eq!(after, target);
    }

    #[test]
    fn the_trim_law_points_the_right_way() {
        let device_rate = 48_000u32;
        let (src, _ring) = capture_source_with_ring(device_rate, None, &[]);

        // Above target: consume faster, so below nominal.
        assert!(src.resampler_trim_for(RING_TARGET_MS + 400) < 1.0);
        // Below target: consume slower, so above nominal.
        assert!(src.resampler_trim_for(0) > 1.0);
        // At target: exactly nominal, no pitch change at all.
        assert_eq!(src.resampler_trim_for(RING_TARGET_MS), 1.0);
    }

    #[test]
    fn the_trim_law_has_a_deadband_and_a_ceiling() {
        let device_rate = 48_000u32;
        let (src, _ring) = capture_source_with_ring(device_rate, None, &[]);

        // Ordinary jitter must not move the ratio, or it would re-ramp
        // constantly for no reason.
        assert_eq!(src.resampler_trim_for(RING_TARGET_MS + RING_DEADBAND_MS), 1.0);
        assert_eq!(
            src.resampler_trim_for(RING_TARGET_MS.saturating_sub(RING_DEADBAND_MS)),
            1.0
        );

        // And a huge error cannot produce an audible speed change.
        let extreme = src.resampler_trim_for(RING_TARGET_MS + 10_000);
        assert!(
            (extreme - (1.0 - MAX_DRIFT_TRIM)).abs() < 1e-6,
            "clamped to the ceiling, got {extreme}"
        );
        let extreme_low = src.resampler_trim_for(0);
        assert!(extreme_low <= 1.0 + MAX_DRIFT_TRIM + 1e-6);
    }

}
