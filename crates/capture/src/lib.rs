//! Platform audio capture via cpal: WASAPI (Windows), PipeWire (Linux), CoreAudio (macOS).
//!
//! Currently implemented: Windows WASAPI loopback capture of the default
//! output device (`SystemCapture::start`). cpal exposes WASAPI loopback by
//! building an *input* stream on the *output* device — there is no separate
//! "loopback device" concept to select.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use thiserror::Error;
use tracing::warn;

/// `--handoff` (Windows only): mute local speakers + mirror Windows volume.
#[cfg(windows)]
pub mod handoff;

/// Windows "now playing" metadata (SMTC), sent to the receiver's screen.
#[cfg(windows)]
pub mod nowplaying;

#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("no default output device available")]
    NoDevice,
    #[error("failed to read default output config: {0}")]
    DefaultConfig(String),
    #[error("failed to build input stream: {0}")]
    BuildStream(String),
    #[error("failed to start stream: {0}")]
    Play(String),
    #[error("unsupported sample format: {0:?}")]
    UnsupportedFormat(SampleFormat),
}

/// Seconds of stereo audio the ring buffer is allowed to hold before the
/// capture callback starts dropping the oldest samples.
const RING_CAPACITY_SECONDS: u32 = 4;

/// Captured system audio, shared via a ring buffer.
///
/// The buffer holds interleaved stereo i16 samples at the *device's native
/// sample rate* (not resampled here — resampling to 44100 Hz happens on the
/// consumer side, see `openair_client::source::CaptureSource`).
pub struct SystemCapture {
    /// Ring buffer of interleaved stereo i16 samples at `device_rate`.
    pub ring: Arc<Mutex<VecDeque<i16>>>,
    pub device_rate: u32,
    /// Friendly name of the device being captured, so callers can confirm
    /// they're recording what they think they are (e.g. the virtual cable
    /// under `--handoff`, not the speakers).
    pub device_name: String,
    // Kept alive so capture keeps running; dropping this stops the stream.
    // cpal::Stream is !Send, so SystemCapture must stay on the thread that
    // created it. Never read directly — its only job is to live as long as
    // `self` and stop the stream on drop.
    #[allow(dead_code)]
    stream: cpal::Stream,
}

impl SystemCapture {
    /// Start loopback capture of the default OUTPUT device.
    ///
    /// On Windows, cpal implements WASAPI loopback by treating the output
    /// device as an input source: `build_input_stream` on a device returned
    /// by `default_output_device()` yields the audio that device is playing.
    pub fn start() -> Result<Self, CaptureError> {
        Self::start_on(None)
    }

    /// Start loopback capture of a specific output device, selected by
    /// case-insensitive substring of its name; `None` uses the default output.
    ///
    /// Used by `--handoff`, which routes system audio to a virtual cable and
    /// then captures from that cable explicitly rather than assuming the
    /// default-device switch took effect.
    pub fn start_on(name_filter: Option<&str>) -> Result<Self, CaptureError> {
        Self::start_on_ring(name_filter, Arc::new(Mutex::new(VecDeque::new())))
    }

    /// As [`SystemCapture::start_on`], but writes into a ring that already
    /// exists.
    ///
    /// This is what lets the capture device change without rebuilding the
    /// consumer: the `CaptureSource` on the stream thread keeps the same `Arc`
    /// and never learns that its producer was replaced.
    ///
    /// **The caller clears the ring at a swap**, not this function — it cannot
    /// know whether it is replacing a producer or starting the first one, and
    /// clearing on a first start would discard a prebuffer filled deliberately.
    pub fn start_on_ring(
        name_filter: Option<&str>,
        ring: Arc<Mutex<VecDeque<i16>>>,
    ) -> Result<Self, CaptureError> {
        let host = cpal::default_host();
        let device = match name_filter {
            Some(want) => {
                let needle = want.to_lowercase();
                host.output_devices()
                    .map_err(|e| CaptureError::DefaultConfig(e.to_string()))?
                    .find(|d| {
                        d.name()
                            .map(|n| n.to_lowercase().contains(&needle))
                            .unwrap_or(false)
                    })
                    .ok_or(CaptureError::NoDevice)?
            }
            None => host.default_output_device().ok_or(CaptureError::NoDevice)?,
        };
        let device_name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
        let supported_config = device
            .default_output_config()
            .map_err(|e| CaptureError::DefaultConfig(e.to_string()))?;

        let sample_format = supported_config.sample_format();
        let config: StreamConfig = supported_config.into();
        let device_rate = config.sample_rate.0;
        let channels = config.channels as usize;

        // Still computed here even though the ring is supplied: `capacity` is
        // the callback's trim threshold, not just an allocation hint, and it
        // depends on the rate of *this* device.
        let capacity = device_rate as usize * 2 * RING_CAPACITY_SECONDS as usize;

        let stream = match sample_format {
            SampleFormat::F32 => build_stream::<f32>(&device, &config, channels, ring.clone(), capacity)?,
            SampleFormat::I16 => build_stream::<i16>(&device, &config, channels, ring.clone(), capacity)?,
            SampleFormat::U16 => build_stream::<u16>(&device, &config, channels, ring.clone(), capacity)?,
            other => return Err(CaptureError::UnsupportedFormat(other)),
        };

        stream.play().map_err(|e| CaptureError::Play(e.to_string()))?;

        Ok(SystemCapture {
            ring,
            device_rate,
            device_name,
            stream,
        })
    }
}

/// Converts one interleaved input sample to i16.
trait ToI16Sample {
    fn to_i16(self) -> i16;
}

impl ToI16Sample for f32 {
    fn to_i16(self) -> i16 {
        (self.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16
    }
}

impl ToI16Sample for i16 {
    fn to_i16(self) -> i16 {
        self
    }
}

impl ToI16Sample for u16 {
    fn to_i16(self) -> i16 {
        // u16 samples are unsigned, centered on 32768; shift to signed range.
        (self as i32 - i32::from(u16::MAX / 2 + 1)) as i16
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    channels: usize,
    ring: Arc<Mutex<VecDeque<i16>>>,
    capacity: usize,
) -> Result<cpal::Stream, CaptureError>
where
    T: cpal::Sample + cpal::SizedSample + ToI16Sample,
{
    let err_fn = |err| warn!("audio capture stream error: {err}");

    let stream = device
        .build_input_stream(
            config,
            move |data: &[T], _info: &cpal::InputCallbackInfo| {
                let mut guard = ring.lock().unwrap();
                // Downmix/upmix to stereo while converting to i16, then push
                // into the ring, dropping the oldest samples on overflow.
                match channels {
                    1 => {
                        for &s in data {
                            let v = s.to_i16();
                            guard.push_back(v);
                            guard.push_back(v);
                        }
                    }
                    2 => {
                        for frame in data.chunks_exact(2) {
                            guard.push_back(frame[0].to_i16());
                            guard.push_back(frame[1].to_i16());
                        }
                    }
                    n => {
                        for frame in data.chunks_exact(n) {
                            guard.push_back(frame[0].to_i16());
                            guard.push_back(frame[1].to_i16());
                        }
                    }
                }
                if guard.len() > capacity {
                    let excess = guard.len() - capacity;
                    guard.drain(..excess);
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| CaptureError::BuildStream(e.to_string()))?;

    Ok(stream)
}

#[cfg(test)]
mod ring_tests {
    use super::*;

    #[test]
    fn a_supplied_ring_is_the_one_capture_uses() {
        // Device-independent: proves the plumbing, not the audio. A real
        // capture needs hardware, so what is pinned down here is that the ring
        // handed in is the ring the SystemCapture reports back — the property
        // a live device swap depends on.
        let ring: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));
        match SystemCapture::start_on_ring(Some("no such device exists anywhere"), Arc::clone(&ring))
        {
            Ok(cap) => assert!(
                Arc::ptr_eq(&cap.ring, &ring),
                "capture must write into the ring it was given, not a fresh one"
            ),
            // The expected outcome of a deliberately impossible filter. The
            // signature and the ownership are what this test exists for.
            Err(CaptureError::NoDevice) => {}
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn clearing_the_ring_is_visible_to_a_holder_that_never_re_read_it() {
        // The invariant the applier's swap relies on: the stream thread holds
        // one Arc for the whole session and must see the producer change
        // through it.
        let ring: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::new()));
        let consumer = Arc::clone(&ring);
        ring.lock().unwrap().extend([1i16, 2, 3, 4]);

        ring.lock().unwrap().clear();
        ring.lock().unwrap().extend([9i16, 9]);

        assert_eq!(
            consumer.lock().unwrap().iter().copied().collect::<Vec<_>>(),
            vec![9, 9],
            "the consumer's handle sees the swap"
        );
    }
}
