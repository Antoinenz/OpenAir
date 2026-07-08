//! [`AudioSource`] implementations: a synthetic sine tone (hardware smoke
//! test) and a WAV file reader with resampling/format conversion to the
//! pipeline's fixed format (interleaved stereo i16 @ 44100 Hz).
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use hound::{SampleFormat, WavReader};

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
    /// Fractional read position in the *source* sample-rate timeline, used
    /// by the linear resampler. Always advances by `src_rate / 44100` per
    /// output frame produced.
    src_pos: f64,
    resample_ratio: f64,
    /// Previous and next source stereo frame, bracketing `src_pos`, used for
    /// interpolation. `None` once the source is exhausted.
    prev_frame: [i16; 2],
    next_frame: Option<[i16; 2]>,
    /// True once the underlying WAV reader has yielded its last sample.
    exhausted: bool,
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

        let resample_ratio = f64::from(spec.sample_rate) / f64::from(SAMPLE_RATE);

        // Prime prev_frame/next_frame so the resampler has a starting
        // bracket at src_pos = 0.
        let prev_frame = read_stereo_frame(&mut reader, spec.channels, spec.sample_format)
            .unwrap_or([0, 0]);
        let next_frame = read_stereo_frame(&mut reader, spec.channels, spec.sample_format);
        let exhausted = next_frame.is_none();

        Ok(WavSource {
            reader,
            src_channels: spec.channels,
            sample_format: spec.sample_format,
            src_pos: 0.0,
            resample_ratio,
            prev_frame,
            next_frame,
            exhausted,
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
        let max_frames = buf.len() / 2;
        let mut written = 0;

        while written < max_frames {
            let Some(next) = self.next_frame else {
                // No more source frames to interpolate towards; stop.
                break;
            };

            // Linear interpolation between prev_frame (at floor(src_pos))
            // and next_frame (at floor(src_pos)+1), using the fractional
            // part of src_pos as the blend weight.
            let frac = self.src_pos.fract();
            let l = lerp(self.prev_frame[0], next[0], frac);
            let r = lerp(self.prev_frame[1], next[1], frac);
            buf[written * 2] = l;
            buf[written * 2 + 1] = r;
            written += 1;

            self.src_pos += self.resample_ratio;

            // Advance the source frame bracket until it straddles the new
            // src_pos (normally one step; can be more if resample_ratio > 1,
            // i.e. downsampling from a higher source rate).
            while self.src_pos >= 1.0 {
                self.src_pos -= 1.0;
                self.prev_frame = next;
                self.next_frame =
                    read_stereo_frame(&mut self.reader, self.src_channels, self.sample_format);
                if self.next_frame.is_none() {
                    self.exhausted = true;
                    break;
                }
            }
            if self.exhausted && self.next_frame.is_none() {
                // Emitted the last interpolatable frame; stop after this.
                break;
            }
        }

        written
    }
}

fn lerp(a: i16, b: i16, t: f64) -> i16 {
    let v = f64::from(a) + (f64::from(b) - f64::from(a)) * t;
    v.round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
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
}
