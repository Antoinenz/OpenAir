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
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or(CaptureError::NoDevice)?;
        let supported_config = device
            .default_output_config()
            .map_err(|e| CaptureError::DefaultConfig(e.to_string()))?;

        let sample_format = supported_config.sample_format();
        let config: StreamConfig = supported_config.into();
        let device_rate = config.sample_rate.0;
        let channels = config.channels as usize;

        let capacity = device_rate as usize * 2 * RING_CAPACITY_SECONDS as usize;
        let ring: Arc<Mutex<VecDeque<i16>>> = Arc::new(Mutex::new(VecDeque::with_capacity(capacity)));

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
