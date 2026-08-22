//! Sample-rate conversion to the pipeline's fixed 44100 Hz.
//!
//! ## Why this replaced linear interpolation
//!
//! The original resampler interpolated linearly between adjacent source
//! frames. That is cheap and completely wrong for audio: linear interpolation
//! is a very poor low-pass filter, so downsampling through it aliases
//! everything above the new Nyquist frequency back down into the audible band,
//! and attenuates what is left near the top of the spectrum.
//!
//! It mattered because Windows defaults to 48 kHz and the pipeline runs at
//! 44.1 kHz, so **the damaging path was the default one**. Setting a capture
//! device to 44.1 kHz by hand was audibly better, which is what prompted this.
//!
//! ## Shape
//!
//! The interface is pull-based — the caller supplies a closure yielding one
//! interleaved stereo frame at a time — because that is what the two callers
//! need. [`CaptureSource`](crate::source::CaptureSource) pulls from a ring
//! buffer that may run dry mid-call, and `WavSource` pulls from a file
//! incrementally. Neither can hand over a whole chunk up front.
//!
//! rubato is chunk-based underneath, so this adapts between the two: pull
//! exactly as many source frames as the next chunk needs, process, and hold
//! the output until the caller has taken it all.
//!
//! ## Passthrough
//!
//! At 44100 Hz in, **nothing is resampled at all** — frames are copied
//! straight through. Not "resampled with a ratio of 1.0": no filter, no delay,
//! no arithmetic. A device already at the pipeline rate cannot be degraded by
//! this module, and the common matched case stays free.

use std::collections::VecDeque;

use rubato::audioadapter_buffers::number_to_float::InterleavedNumbers;
use rubato::{
    Async, FixedAsync, Indexing, Resampler as _, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};
use tracing::{debug, warn};

use crate::SAMPLE_RATE;

/// Output frames produced per rubato chunk.
///
/// Larger amortises the per-chunk overhead; smaller bounds how much output is
/// held when the caller asks for less than a full chunk. One packet of audio is
/// ~1024 frames, so this lines up with how `fill` is actually called.
const CHUNK_FRAMES: usize = 1024;

/// Stereo throughout: the pipeline has no other shape.
const CHANNELS: usize = 2;

/// Length of the sinc filter, in taps.
///
/// 256 is the transparent end of rubato's useful range. The cost is group
/// delay of about half the filter — ~128 source frames, ~2.7 ms at 48 kHz —
/// which is immaterial against the pipeline's 500 ms default anchor latency.
/// Halving this would halve the delay and still sound excellent, if latency
/// ever becomes the binding constraint.
const SINC_LEN: usize = 256;

/// Cutoff as a fraction of the *lower* of the two Nyquist frequencies.
///
/// Below 1.0 deliberately: pushing the cutoff to Nyquist demands an
/// impossibly steep transition and rings. 0.95 keeps everything to ~21 kHz on
/// a 44.1 kHz output, which is past the top of human hearing.
const F_CUTOFF: f32 = 0.95;

/// Converts interleaved stereo i16 at some source rate to interleaved stereo
/// i16 at [`SAMPLE_RATE`].
pub(crate) struct Resampler {
    mode: Mode,
    /// Resampled frames produced but not yet handed to a caller. rubato emits
    /// whole chunks; callers ask for arbitrary amounts.
    pending: VecDeque<[i16; 2]>,
    /// The source returned `None`. No more input will be pulled.
    source_done: bool,
    /// The tail has been flushed out of the filter after `source_done`.
    flushed: bool,
}

enum Mode {
    /// Source is already at the pipeline rate: copy, do not filter.
    Passthrough,
    Sinc(Box<SincState>),
}

struct SincState {
    inner: Async<f32>,
    src_rate: u32,
    /// Interleaved i16 scratch, reused every chunk so the audio path does not
    /// allocate. rubato works in f32 internally; the `InterleavedNumbers`
    /// adapter converts on read and write, so nothing here is planar and
    /// nothing is scaled by hand.
    input: Vec<i16>,
    /// Valid frames currently in `input`. A chunk can be filled across several
    /// `fill` calls when a live source runs dry part-way through one.
    input_frames: usize,
    output: Vec<i16>,
}

impl Resampler {
    /// Build a resampler from `src_rate` to [`SAMPLE_RATE`].
    ///
    /// Falls back to passthrough if rubato rejects the ratio. That is the
    /// wrong *pitch* rather than silence, and it is logged loudly — but a
    /// stream that plays at the wrong speed is still recoverable by the user,
    /// whereas one that refuses to start is not.
    pub(crate) fn new(src_rate: u32) -> Self {
        let mode = if src_rate == SAMPLE_RATE {
            debug!(rate = src_rate, "source is at the pipeline rate — no resampling");
            Mode::Passthrough
        } else {
            match build_sinc(src_rate) {
                Some(state) => {
                    debug!(
                        from_hz = src_rate,
                        to_hz = SAMPLE_RATE,
                        taps = SINC_LEN,
                        "resampling with a windowed sinc"
                    );
                    Mode::Sinc(Box::new(state))
                }
                None => {
                    warn!(
                        rate = src_rate,
                        "could not build a resampler for this rate — passing audio \
                         through unconverted, which will play at the wrong speed"
                    );
                    Mode::Passthrough
                }
            }
        };
        Resampler {
            mode,
            pending: VecDeque::new(),
            source_done: false,
            flushed: false,
        }
    }

    /// The source rate currently configured, or [`SAMPLE_RATE`] in
    /// passthrough.
    pub(crate) fn src_rate(&self) -> u32 {
        match &self.mode {
            Mode::Passthrough => SAMPLE_RATE,
            Mode::Sinc(s) => s.src_rate,
        }
    }

    /// Switch to a new source rate, discarding filter state.
    ///
    /// Called when the capture device changes underneath a live stream. The
    /// ring is cleared at the same moment, so there is nothing in flight worth
    /// preserving — and carrying a filter's history across a device change
    /// would smear one device's audio into the other's.
    pub(crate) fn set_rate(&mut self, src_rate: u32) {
        if src_rate == self.src_rate() {
            return;
        }
        *self = Resampler::new(src_rate);
    }

    /// True once no further output can be produced.
    pub(crate) fn is_exhausted(&self) -> bool {
        self.source_done && self.flushed && self.pending.is_empty()
    }

    /// Re-arm after exhaustion, so a caller whose source has more data later
    /// (a capture ring that refilled) can keep going.
    pub(crate) fn rearm(&mut self) {
        self.source_done = false;
        self.flushed = false;
    }

    /// Produce up to `buf.len() / 2` frames, pulling source frames as needed.
    /// Returns frames written.
    pub(crate) fn fill(
        &mut self,
        buf: &mut [i16],
        mut next_source_frame: impl FnMut() -> Option<[i16; 2]>,
    ) -> usize {
        let want = buf.len() / 2;
        if want == 0 {
            return 0;
        }

        if let Mode::Passthrough = self.mode {
            return self.fill_passthrough(buf, want, &mut next_source_frame);
        }

        while self.pending.len() < want && !self.is_exhausted() {
            if !self.produce_chunk(&mut next_source_frame) {
                break;
            }
        }

        let n = want.min(self.pending.len());
        for i in 0..n {
            let f = self.pending.pop_front().expect("checked length");
            buf[i * 2] = f[0];
            buf[i * 2 + 1] = f[1];
        }
        n
    }

    fn fill_passthrough(
        &mut self,
        buf: &mut [i16],
        want: usize,
        next_source_frame: &mut impl FnMut() -> Option<[i16; 2]>,
    ) -> usize {
        let mut written = 0;
        while written < want {
            match next_source_frame() {
                Some(f) => {
                    buf[written * 2] = f[0];
                    buf[written * 2 + 1] = f[1];
                    written += 1;
                }
                None => {
                    self.source_done = true;
                    self.flushed = true;
                    break;
                }
            }
        }
        written
    }

    /// Pull one chunk's worth of input and process it. Returns whether any
    /// output was produced.
    fn produce_chunk(
        &mut self,
        next_source_frame: &mut impl FnMut() -> Option<[i16; 2]>,
    ) -> bool {
        let Mode::Sinc(state) = &mut self.mode else {
            return false;
        };

        let needed = state.inner.input_frames_next();
        while state.input_frames < needed {
            match next_source_frame() {
                Some(f) => {
                    let base = state.input_frames * CHANNELS;
                    state.input[base] = f[0];
                    state.input[base + 1] = f[1];
                    state.input_frames += 1;
                }
                None => {
                    self.source_done = true;
                    break;
                }
            }
        }

        // Short of a full chunk with the source still live: keep what has been
        // pulled and come back for the rest. Padding with silence here would
        // punch a gap into continuous audio every time a ring momentarily ran
        // dry, which on a live capture is often.
        if state.input_frames < needed && !self.source_done {
            return false;
        }

        if state.input_frames == 0 {
            self.flushed = true;
            return false;
        }

        // A partial final chunk is declared through `Indexing`, which tells
        // rubato how many frames are real and to treat the rest as silence --
        // that is what flushing a filter's tail means.
        let partial = state.input_frames < needed;
        // How many output frames the *real* input is worth. Past the end of a
        // finite source rubato still emits a whole chunk, the tail of it being
        // the zero padding we just added -- for a file that is up to ~23 ms of
        // silence appended to the stream, and it breaks the frame-count
        // contract callers rely on.
        let real_out = if partial {
            Some(
                ((state.input_frames as f64) * f64::from(SAMPLE_RATE) / f64::from(state.src_rate))
                    .round() as usize,
            )
        } else {
            None
        };
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: partial.then_some(state.input_frames),
            active_channels_mask: None,
        };
        if partial {
            self.flushed = true;
        }

        let in_frames = needed.max(state.input_frames);
        let adapter_in =
            match InterleavedNumbers::<&[i16], f32>::new(&state.input, CHANNELS, in_frames) {
                Ok(a) => a,
                Err(e) => {
                    warn!("resampler input adapter rejected a chunk (dropping it): {e}");
                    state.input_frames = 0;
                    return false;
                }
            };
        let out_frames = state.output.len() / CHANNELS;
        let mut adapter_out = match InterleavedNumbers::<&mut [i16], f32>::new_mut(
            &mut state.output,
            CHANNELS,
            out_frames,
        ) {
            Ok(a) => a,
            Err(e) => {
                warn!("resampler output adapter rejected a chunk (dropping it): {e}");
                state.input_frames = 0;
                return false;
            }
        };

        let result = state
            .inner
            .process_into_buffer(&adapter_in, &mut adapter_out, Some(&indexing));
        state.input_frames = 0;

        let (_, produced) = match result {
            Ok(counts) => counts,
            Err(e) => {
                warn!("resampler failed on a chunk (dropping it): {e}");
                return false;
            }
        };

        let keep = real_out.map_or(produced, |r| r.min(produced));
        for i in 0..keep {
            self.pending
                .push_back([state.output[i * CHANNELS], state.output[i * CHANNELS + 1]]);
        }
        keep > 0
    }
}

fn build_sinc(src_rate: u32) -> Option<SincState> {
    if src_rate == 0 {
        return None;
    }
    let params = SincInterpolationParameters {
        sinc_len: SINC_LEN,
        f_cutoff: Some(F_CUTOFF),
        interpolation: SincInterpolationType::Cubic,
        oversampling_factor: 256,
        window: WindowFunction::BlackmanHarris2,
    };
    // `FixedAsync::Output` -- a fixed number of frames *out* per chunk, with
    // however many in are needed. That is the direction `fill` works in: the
    // caller asks for a buffer's worth of output.
    let inner = Async::<f32>::new_sinc(
        f64::from(SAMPLE_RATE) / f64::from(src_rate),
        1.0,
        &params,
        CHUNK_FRAMES,
        CHANNELS,
        FixedAsync::Output,
    )
    .ok()?;

    let in_cap = inner.input_frames_max() * CHANNELS;
    let out_cap = inner.output_frames_max() * CHANNELS;
    Some(SincState {
        inner,
        src_rate,
        input: vec![0; in_cap],
        input_frames: 0,
        output: vec![0; out_cap],
    })
}

// Note on quantisation: the `InterleavedNumbers` adapter converts f32 back
// to i16 on write, clamping as it goes. No dither is applied -- the source is
// already 16-bit, so quantisation error sits around -96 dBFS, and dithering
// would trade inaudible distortion for a real noise floor.

#[cfg(test)]
mod tests {
    use super::*;

    /// Interleaved stereo sine at `freq` Hz sampled at `rate`.
    fn tone(freq: f64, rate: u32, frames: usize) -> Vec<[i16; 2]> {
        (0..frames)
            .map(|i| {
                let t = i as f64 / f64::from(rate);
                let v = (t * freq * std::f64::consts::TAU).sin() * 0.5;
                let s = (v * 32767.0).round() as i16;
                [s, s]
            })
            .collect()
    }

    fn run(src_rate: u32, input: &[[i16; 2]], want_frames: usize) -> Vec<[i16; 2]> {
        let mut r = Resampler::new(src_rate);
        let mut iter = input.iter().copied();
        let mut out = Vec::new();
        let mut buf = vec![0i16; 2048];
        while out.len() < want_frames {
            let n = r.fill(&mut buf, || iter.next());
            if n == 0 {
                break;
            }
            for i in 0..n {
                out.push([buf[i * 2], buf[i * 2 + 1]]);
            }
        }
        out.truncate(want_frames);
        out
    }

    /// Level of a block of frames, in dBFS.
    ///
    /// Deliberately delay- and phase-insensitive: a good resampler has group
    /// delay (~2.7 ms here), so anything comparing sample-against-sample with
    /// an ideal tone measures the delay rather than the quality. At 15 kHz a
    /// single sample of skew is most of a cycle.
    fn rms_dbfs(x: &[[i16; 2]], skip: usize) -> f64 {
        let mut sum = 0.0;
        let mut n = 0.0;
        for f in &x[skip..] {
            let v = f64::from(f[0]) / 32768.0;
            sum += v * v;
            n += 1.0;
        }
        20.0 * (sum / n).sqrt().max(1e-12).log10()
    }

    /// The old algorithm, reproduced so the claims below can be checked
    /// against it rather than asserted.
    fn linear_resample(input: &[[i16; 2]], src_rate: u32, want: usize) -> Vec<[i16; 2]> {
        let ratio = f64::from(src_rate) / f64::from(SAMPLE_RATE);
        let mut pos = 0.0f64;
        let mut out = Vec::new();
        while (pos as usize) + 1 < input.len() && out.len() < want {
            let i = pos as usize;
            let f = pos.fract();
            let a = f64::from(input[i][0]);
            let b = f64::from(input[i + 1][0]);
            let v = (a + (b - a) * f).round() as i16;
            out.push([v, v]);
            pos += ratio;
        }
        out
    }

    /// A tone at 0.5 full scale has this level; both tests below compare
    /// against it rather than a magic number.
    const INPUT_DBFS: f64 = -9.03;

    #[test]
    fn a_passband_tone_comes_through_at_full_level() {
        // 15 kHz is inside 44.1 kHz's passband, so it should survive 48 ->
        // 44.1 essentially untouched. Linear interpolation is a sinc-squared
        // low-pass, so it audibly dulls exactly this region -- which is what
        // made matching the device rate by hand sound better.
        let input = tone(15_000.0, 48_000, 48_000);
        let out = run(48_000, &input, 40_000);
        assert!(out.len() > 30_000, "produced {} frames", out.len());

        let level = rms_dbfs(&out, 4_000);
        assert!(
            (level - INPUT_DBFS).abs() < 1.0,
            "15 kHz should pass at full level, got {level:.1} dBFS vs {INPUT_DBFS:.1} in"
        );

        // And the bar means something: the old approach loses real level here.
        let old = linear_resample(&input, 48_000, 40_000);
        let old_level = rms_dbfs(&old, 4_000);
        assert!(
            level > old_level + 1.0,
            "sinc {level:.1} dBFS should beat linear {old_level:.1} dBFS at 15 kHz"
        );
    }

    #[test]
    fn a_tone_above_the_output_nyquist_is_rejected_not_folded() {
        // The actual defect. 23 kHz cannot exist at 44.1 kHz (Nyquist 22.05),
        // so it must be filtered away. Linear interpolation does not filter,
        // so it folds down to ~21.1 kHz -- an audible tone that was never in
        // the source. This is aliasing, and it is why the old resampler
        // sounded wrong rather than merely dull.
        let input = tone(23_000.0, 48_000, 48_000);
        let out = run(48_000, &input, 40_000);
        assert!(out.len() > 30_000, "produced {} frames", out.len());

        let level = rms_dbfs(&out, 4_000);
        assert!(
            level < INPUT_DBFS - 40.0,
            "23 kHz should be rejected, got {level:.1} dBFS (input {INPUT_DBFS:.1})"
        );

        let old = linear_resample(&input, 48_000, 40_000);
        let old_level = rms_dbfs(&old, 4_000);
        assert!(
            old_level > level + 20.0,
            "linear was expected to fold this into the band: linear {old_level:.1} \
             vs sinc {level:.1} dBFS"
        );
    }

    #[test]
    fn upsampling_preserves_level() {
        // Not the motivating case, but 32 kHz devices exist.
        let input = tone(1_000.0, 32_000, 32_000);
        let out = run(32_000, &input, 40_000);
        assert!(out.len() > 39_000, "produced {} frames", out.len());
        let level = rms_dbfs(&out, 4_000);
        assert!(
            (level - INPUT_DBFS).abs() < 1.0,
            "1 kHz upsampled should keep its level, got {level:.1} dBFS"
        );
    }

    #[test]
    fn output_length_tracks_the_ratio() {
        // 48000 -> 44100 is 0.91875; a second of input is ~44100 frames out.
        let input = tone(440.0, 48_000, 48_000);
        let out = run(48_000, &input, 100_000);
        let expected = 44_100.0;
        let ratio = out.len() as f64 / expected;
        assert!(
            (0.95..=1.05).contains(&ratio),
            "expected ~{expected} frames, got {}",
            out.len()
        );
    }

    #[test]
    fn a_dry_source_stops_cleanly_and_can_resume() {
        // The live case: a capture ring runs dry mid-call and refills. This
        // must not be mistaken for end-of-stream.
        let mut r = Resampler::new(48_000);
        let mut buf = vec![0i16; 512];

        let n = r.fill(&mut buf, || None);
        assert_eq!(n, 0, "nothing in, nothing out");

        let input = tone(440.0, 48_000, 20_000);
        let mut iter = input.iter().copied();
        r.rearm();
        let n = r.fill(&mut buf, || iter.next());
        assert!(n > 0, "should resume once the source has data again");
    }

    #[test]
    fn set_rate_switches_and_is_a_no_op_when_unchanged() {
        let mut r = Resampler::new(48_000);
        assert_eq!(r.src_rate(), 48_000);
        r.set_rate(48_000);
        assert_eq!(r.src_rate(), 48_000);
        r.set_rate(SAMPLE_RATE);
        assert_eq!(r.src_rate(), SAMPLE_RATE, "switched to passthrough");
    }

    #[test]
    fn an_absurd_rate_falls_back_to_passthrough_rather_than_failing() {
        // Wrong pitch is recoverable by the user; a stream that will not start
        // is not.
        let r = Resampler::new(0);
        assert_eq!(r.src_rate(), SAMPLE_RATE);
    }

    #[test]
    fn a_zero_length_buffer_is_handled() {
        let mut r = Resampler::new(48_000);
        assert_eq!(r.fill(&mut [], || Some([0, 0])), 0);
    }
}
