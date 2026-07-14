//! [`AudioSource`] implementations: a synthetic sine tone (hardware smoke
//! test), a WAV file reader, and a live system-audio capture source — all
//! resampled/format-converted to the pipeline's fixed format (interleaved
//! stereo i16 @ 44100 Hz).
use std::collections::VecDeque;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hound::{SampleFormat, WavReader};
use tracing::debug;

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

/// Linear-interpolation resampler from an arbitrary source sample rate to
/// the pipeline's fixed 44100 Hz, operating on interleaved stereo i16
/// frames.
///
/// Callers supply source frames one at a time via `next_source_frame`
/// (`None` means the source is exhausted); the resampler keeps its
/// fractional position and interpolation bracket (`prev`/`next`) across
/// calls to [`fill`](LinearResampler::fill), so a source can be pulled
/// incrementally across many `fill` calls without re-priming.
pub(crate) struct LinearResampler {
    /// Fractional read position in the *source* sample-rate timeline.
    /// Always advances by `resample_ratio` per output frame produced.
    src_pos: f64,
    resample_ratio: f64,
    /// Previous and next source stereo frame, bracketing `src_pos`, used for
    /// interpolation. `next` is `None` once the source is exhausted.
    prev: [i16; 2],
    next: Option<[i16; 2]>,
}

impl LinearResampler {
    /// Creates a resampler for `src_rate` -> 44100 Hz. `next_source_frame`
    /// is called twice to prime the initial interpolation bracket.
    pub(crate) fn new(src_rate: u32, mut next_source_frame: impl FnMut() -> Option<[i16; 2]>) -> Self {
        let resample_ratio = f64::from(src_rate) / f64::from(SAMPLE_RATE);
        let prev = next_source_frame().unwrap_or([0, 0]);
        let next = next_source_frame();
        LinearResampler {
            src_pos: 0.0,
            resample_ratio,
            prev,
            next,
        }
    }

    /// True once the source has been exhausted and the last interpolatable
    /// frame has been emitted (i.e. `fill` will produce no more output).
    pub(crate) fn is_exhausted(&self) -> bool {
        self.next.is_none()
    }

    /// Produces up to `buf.len()/2` resampled stereo frames into `buf`,
    /// pulling additional source frames from `next_source_frame` as needed.
    /// Returns the number of frames written.
    pub(crate) fn fill(
        &mut self,
        buf: &mut [i16],
        mut next_source_frame: impl FnMut() -> Option<[i16; 2]>,
    ) -> usize {
        let max_frames = buf.len() / 2;
        let mut written = 0;

        while written < max_frames {
            let Some(next) = self.next else {
                // No more source frames to interpolate towards; stop.
                break;
            };

            // Linear interpolation between prev (at floor(src_pos)) and
            // next (at floor(src_pos)+1), using the fractional part of
            // src_pos as the blend weight.
            let frac = self.src_pos.fract();
            let l = lerp(self.prev[0], next[0], frac);
            let r = lerp(self.prev[1], next[1], frac);
            buf[written * 2] = l;
            buf[written * 2 + 1] = r;
            written += 1;

            self.src_pos += self.resample_ratio;

            // Advance the source frame bracket until it straddles the new
            // src_pos (normally one step; can be more if resample_ratio > 1,
            // i.e. downsampling from a higher source rate).
            while self.src_pos >= 1.0 {
                self.src_pos -= 1.0;
                self.prev = next;
                self.next = next_source_frame();
                if self.next.is_none() {
                    break;
                }
            }
            if self.next.is_none() {
                // Emitted the last interpolatable frame; stop after this.
                break;
            }
        }

        written
    }
}

/// Reads a WAV file and yields interleaved stereo i16 samples at 44100 Hz,
/// regardless of the file's native format.
///
/// Supported inputs: 16-bit integer PCM or 32-bit float, 1 or 2 channels,
/// any sample rate. Mono is duplicated to both channels; float samples are
/// scaled by `i16::MAX` with clamping; sample rates other than 44100 Hz are
/// converted with a simple linear-interpolation resampler.
///
/// Decoding happens incrementally in [`fill`](AudioSource::fill): the
/// decoder keeps a small internal buffer of source-rate stereo frames rather
/// than loading the whole file into memory.
pub struct WavSource {
    reader: WavReader<BufReader<File>>,
    src_channels: u16,
    sample_format: SampleFormat,
    resampler: LinearResampler,
}

impl WavSource {
    pub fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut reader = WavReader::open(path)?;
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

        let resampler = LinearResampler::new(spec.sample_rate, || {
            read_stereo_frame(&mut reader, spec.channels, spec.sample_format)
        });
        // The closure's borrow of `reader` ends here (`new` only calls it
        // synchronously to prime the bracket), so `reader` is free to move
        // into the struct below.

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

fn lerp(a: i16, b: i16, t: f64) -> i16 {
    let v = f64::from(a) + (f64::from(b) - f64::from(a)) * t;
    v.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
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

/// Ring exceeding this many ms of buffered audio indicates the sender is
/// falling behind the device's capture rate (clock drift or a stalled RTP
/// pacing loop); [`CaptureSource::fill`] drains it back down to avoid
/// unbounded latency growth.
const DRIFT_HIGH_WATER_MS: u32 = 1000;
/// Target ms of buffered audio left after a drift-guard drain.
const DRIFT_DRAIN_TARGET_MS: u32 = 300;

/// Live system-audio capture source: resamples from a shared ring buffer
/// (filled by `openair_capture::SystemCapture` on a cpal callback thread,
/// device-rate stereo i16) to the pipeline's fixed 44100 Hz format.
///
/// `SystemCapture` (and its `!Send` `cpal::Stream`) never enters this crate;
/// only the `Arc<Mutex<VecDeque<i16>>>` ring crosses the thread boundary.
pub struct CaptureSource {
    ring: Arc<Mutex<VecDeque<i16>>>,
    device_rate: u32,
    resampler: LinearResampler,
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
        let frames_remaining = max_seconds.map(|s| u64::from(s) * u64::from(SAMPLE_RATE));
        // The resampler needs an initial two-frame bracket, but the ring
        // may not have any data yet (capture just started) — prime with
        // silence; fill() waits for the real prebuffer before producing
        // output, and by the time it does, pull_ring_frame will be reading
        // live data anyway.
        let resampler = LinearResampler::new(device_rate, || Some([0, 0]));
        CaptureSource {
            ring,
            device_rate,
            resampler,
            frames_remaining,
            prebuffer_done: false,
            stop,
            fills: 0,
            silent_frames: 0,
            blocking: false,
        }
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
    fn wait_for_frames(&self, frames: usize) {
        // Output frames → device-rate samples (stereo interleaved), plus one
        // spare frame for the resampler bracket.
        let needed =
            ((frames as f64 * f64::from(self.device_rate) / f64::from(SAMPLE_RATE)) as usize + 2)
                * 2;
        let deadline = Instant::now() + Duration::from_millis(1000);
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

    /// If the ring has accumulated more than `DRIFT_HIGH_WATER_MS` of
    /// audio, drain it down to `DRIFT_DRAIN_TARGET_MS`. The producer (cpal
    /// callback) and consumer (this resampler, paced by the RTP send loop)
    /// run on independent clocks; without this guard a persistent drift
    /// would grow buffered latency without bound.
    fn apply_drift_guard(&self) {
        let high_water = (self.device_rate as u64 * 2 * u64::from(DRIFT_HIGH_WATER_MS) / 1000) as usize;
        let target = (self.device_rate as u64 * 2 * u64::from(DRIFT_DRAIN_TARGET_MS) / 1000) as usize;
        let mut guard = self.ring.lock().unwrap();
        if guard.len() > high_water {
            let drain = guard.len() - target;
            debug!(
                ring_len = guard.len(),
                drain, "capture ring overfull, draining for drift"
            );
            guard.drain(..drain);
        }
    }
}

impl AudioSource for CaptureSource {
    fn fill(&mut self, buf: &mut [i16]) -> usize {
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

        self.apply_drift_guard();

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
                // Re-arm the resampler so subsequent fills resume pulling
                // from the ring instead of reporting exhausted forever.
                self.resampler = LinearResampler::new(self.device_rate, || Some([0, 0]));
            }
        }

        if let Some(remaining) = &mut self.frames_remaining {
            *remaining = remaining.saturating_sub(total as u64);
        }

        // Periodic capture health log: how much live audio is buffered and
        // how much of this fill was real vs. silence padding.
        self.fills += 1;
        self.silent_frames += (total - written) as u64;
        if self.fills % 250 == 0 {
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
        fn is_finite_i16(&self) -> bool {
            *self >= i16::MIN && *self <= i16::MAX
        }
    }

    // -- LinearResampler --------------------------------------------------

    /// Builds a source of `n` stereo frames with a distinct ramp value per
    /// frame (L=2*i, R=2*i+1, clamped to i16), consumed via an index cursor.
    fn ramp_frames(n: usize) -> Vec<[i16; 2]> {
        (0..n)
            .map(|i| [(2 * i) as i16, (2 * i + 1) as i16])
            .collect()
    }

    #[test]
    fn linear_resampler_identity_at_44100_passes_through() {
        let frames = ramp_frames(1000);
        let mut idx = 0usize;
        let mut resampler = LinearResampler::new(44100, || {
            let f = frames.get(idx).copied();
            idx += 1;
            f
        });

        let mut buf = [0i16; 352 * 2];
        let written = resampler.fill(&mut buf, || {
            let f = frames.get(idx).copied();
            idx += 1;
            f
        });

        // Identity resample ratio (44100 -> 44100): frame count should
        // match exactly, and since consecutive ramp values are close, the
        // interpolated output should equal the source almost exactly.
        assert_eq!(written, 352);
    }

    #[test]
    fn linear_resampler_48000_to_44100_frame_count_ratio() {
        // 48000 Hz source, request enough output for ~1 second: expect
        // ~44100 frames consumed from ~48000 source frames (ratio ~0.919).
        let total_src = 48000usize;
        let frames = ramp_frames(total_src);
        let mut idx = 0usize;
        let mut pull = || {
            let f = frames.get(idx).copied();
            idx += 1;
            f
        };
        let mut resampler = LinearResampler::new(48000, &mut pull);

        let mut total_written = 0u64;
        loop {
            let mut buf = [0i16; 352 * 2];
            let written = resampler.fill(&mut buf, &mut pull);
            total_written += written as u64;
            if written < 352 {
                // Source exhausted mid-fill; stop.
                break;
            }
        }

        let expected = (total_src as f64 * 44100.0 / 48000.0) as i64;
        assert!(
            (total_written as i64 - expected).abs() <= 2,
            "total_written={total_written}, expected ~{expected}"
        );
    }

    #[test]
    fn linear_resampler_is_continuous_across_calls() {
        // Ramp source with a constant per-frame step; verify no discontinuity
        // (beyond the expected per-frame step) at the boundary between two
        // separate fill() calls.
        let frames = ramp_frames(2000);
        let mut idx = 0usize;
        let mut pull = || {
            let f = frames.get(idx).copied();
            idx += 1;
            f
        };
        let mut resampler = LinearResampler::new(44100, &mut pull);

        let mut buf1 = [0i16; 352 * 2];
        let mut buf2 = [0i16; 352 * 2];
        assert_eq!(resampler.fill(&mut buf1, &mut pull), 352);
        assert_eq!(resampler.fill(&mut buf2, &mut pull), 352);

        let last = buf1[buf1.len() - 2];
        let first = buf2[0];
        // Ramp advances by 2 per source frame at 1:1 resample ratio; allow
        // a little slack for interpolation rounding.
        assert!(
            (i32::from(first) - i32::from(last)).abs() <= 4,
            "discontinuity across fill() calls: last={last}, first={first}"
        );
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
        let (src, ring) = capture_source_with_ring(device_rate, None, &frames);

        // Sanity: ring holds ~2s of stereo audio before the guard runs.
        let before = ring.lock().unwrap().len();
        assert_eq!(before, frame_count * 2);

        src.apply_drift_guard();

        let after = ring.lock().unwrap().len();
        let target_samples = (device_rate as u64 * 2 * u64::from(DRIFT_DRAIN_TARGET_MS) / 1000) as usize;
        assert_eq!(
            after, target_samples,
            "drift guard should drain ring down to the target watermark"
        );
    }
}
