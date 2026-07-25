//! `--handoff` support (Windows only): mute the local speakers while streaming
//! and mirror the Windows master volume onto AirPlay.
//!
//! [`VolumeBridge::start`] mutes the default render endpoint (so audio only
//! comes out of the AirPlay receiver — WASAPI loopback taps the mix *before*
//! endpoint mute, so capture keeps delivering full-scale audio) and spawns a
//! poll thread that watches the Windows master volume/mute. User changes are
//! translated to AirPlay dBFS and delivered on the returned channel; the caller
//! forwards them to every receiver via RTSP `SET_PARAMETER volume`.
//!
//! Approach A (polling). The `start → Receiver → Drop` surface is deliberately
//! independent of *how* changes are observed, so a future event-driven upgrade
//! (`IAudioEndpointVolumeCallback`, approach B in the design doc) stays internal.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::JoinHandle;
use std::time::Duration;

use thiserror::Error;
use tracing::{info, warn};

use windows::Win32::Foundation::BOOL;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};

/// How often the poll thread samples the Windows master volume/mute.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// dBFS value meaning "silent" (matches RTSP `set_volume`'s mute sentinel).
const MUTE_DBFS: f32 = -144.0;

/// Quietest non-muted AirPlay level we map to; `20·log10(0.0316) ≈ -30`.
const MIN_DBFS: f32 = -30.0;

/// Scalars within this of each other are treated as unchanged (Windows volume
/// scalars are quantized; this avoids spurious events without missing a real
/// one-step change, which is ~0.02).
const SCALAR_EPS: f32 = 1e-4;

/// A mirrored-volume update to forward to the AirPlay receivers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VolumeEvent {
    /// New AirPlay level in dBFS. [`MUTE_DBFS`] (`-144`) means silent.
    Level(f32),
}

#[derive(Debug, Error)]
pub enum HandoffError {
    #[error("COM initialization failed: {0}")]
    ComInit(String),
    #[error("could not get default render endpoint: {0}")]
    Endpoint(String),
    #[error("could not access endpoint volume: {0}")]
    Activate(String),
}

/// Converts a Windows master-volume scalar (`0.0..=1.0`) to AirPlay dBFS,
/// clamped to `[-30, 0]`; `0.0` (or below) maps to [`MUTE_DBFS`] (silent).
fn scalar_to_dbfs(scalar: f32) -> f32 {
    if scalar <= 0.0 {
        return MUTE_DBFS;
    }
    let db = 20.0 * scalar.log10();
    db.clamp(MIN_DBFS, 0.0)
}

/// Poll-to-poll state for the mirror state machine (pure — no COM).
#[derive(Debug, Clone, Copy, PartialEq)]
struct MirrorState {
    /// Last-seen Windows master scalar.
    last_scalar: f32,
    /// dBFS for the last audible scalar (restored when the user un-mutes).
    audible_db: f32,
    /// Our logical AirPlay-mute intent, toggled by the user's mute key.
    airplay_muted: bool,
}

impl MirrorState {
    fn initial(scalar: f32) -> Self {
        MirrorState {
            last_scalar: scalar,
            audible_db: scalar_to_dbfs(scalar),
            airplay_muted: false,
        }
    }
}

/// What the poll thread should do after one sample.
#[derive(Debug, Clone, Copy, PartialEq)]
struct PollAction {
    /// Event to emit (if any).
    emit: Option<VolumeEvent>,
    /// Whether we must re-assert the speaker mute (Windows auto-unmutes on a
    /// volume-key press, and a user mute-toggle unmutes the endpoint).
    reassert_mute: bool,
    /// State to carry into the next sample.
    next: MirrorState,
}

/// Classify one poll sample `(cur_scalar, cur_muted)` against `prev`.
///
/// Because we hold the endpoint muted, the only mute-flag transition a user can
/// produce is `true → false` (they press mute/unmute). We read that gesture:
/// - **scalar changed** → the user moved the volume → emit the new level and
///   re-assert mute if Windows cleared it; this also makes AirPlay audible.
/// - **scalar unchanged but endpoint now unmuted** → the user pressed the mute
///   key → toggle our AirPlay-mute intent, emit `-144`/last level, re-mute.
/// - **otherwise** (steady state, our mute holding) → nothing.
fn classify(prev: &MirrorState, cur_scalar: f32, cur_muted: bool) -> PollAction {
    if (cur_scalar - prev.last_scalar).abs() > SCALAR_EPS {
        let db = scalar_to_dbfs(cur_scalar);
        return PollAction {
            emit: Some(VolumeEvent::Level(db)),
            reassert_mute: !cur_muted,
            next: MirrorState {
                last_scalar: cur_scalar,
                audible_db: db,
                airplay_muted: false,
            },
        };
    }
    if !cur_muted {
        // Endpoint unmuted with the same scalar → user tapped the mute key.
        let now_muted = !prev.airplay_muted;
        let db = if now_muted { MUTE_DBFS } else { prev.audible_db };
        return PollAction {
            emit: Some(VolumeEvent::Level(db)),
            reassert_mute: true,
            next: MirrorState {
                airplay_muted: now_muted,
                ..*prev
            },
        };
    }
    PollAction {
        emit: None,
        reassert_mute: false,
        next: *prev,
    }
}

/// Live handle to the volume bridge. Dropping it stops the poll thread, which
/// restores the endpoint's original volume + mute before exiting.
pub struct VolumeBridge {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl VolumeBridge {
    /// Mute the default render endpoint, stash its original volume + mute, and
    /// start mirroring Windows volume changes. Returns the bridge (keep it
    /// alive for the stream's lifetime) and a channel of [`VolumeEvent`]s.
    pub fn start() -> Result<(VolumeBridge, Receiver<VolumeEvent>), HandoffError> {
        let (event_tx, event_rx) = std::sync::mpsc::channel::<VolumeEvent>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), HandoffError>>();
        let stop = Arc::new(AtomicBool::new(false));

        let thread_stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name("handoff-volume".into())
            .spawn(move || run(thread_stop, ready_tx, event_tx))
            .expect("spawn handoff thread");

        // Wait for the thread to finish COM setup so we can surface a clean
        // error (and degrade off) instead of returning a half-started bridge.
        match ready_rx.recv() {
            Ok(Ok(())) => Ok((
                VolumeBridge {
                    stop,
                    handle: Some(handle),
                },
                event_rx,
            )),
            Ok(Err(e)) => {
                let _ = handle.join();
                Err(e)
            }
            Err(_) => {
                let _ = handle.join();
                Err(HandoffError::ComInit("bridge thread exited early".into()))
            }
        }
    }
}

impl Drop for VolumeBridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// COM interfaces + the original state to restore, all owned by the poll thread
/// (COM objects have apartment affinity — they never leave this thread).
struct Endpoint {
    vol: IAudioEndpointVolume,
    orig_scalar: f32,
    orig_muted: BOOL,
}

/// Acquire the default render endpoint's volume control, read + stash its
/// original state, and mute it. Runs on the poll thread (COM already init'd).
fn setup_endpoint() -> Result<Endpoint, HandoffError> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| HandoffError::ComInit(e.to_string()))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| HandoffError::Endpoint(e.to_string()))?;
        let vol: IAudioEndpointVolume = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| HandoffError::Activate(e.to_string()))?;

        let orig_scalar = vol
            .GetMasterVolumeLevelScalar()
            .map_err(|e| HandoffError::Activate(e.to_string()))?;
        let orig_muted = vol
            .GetMute()
            .map_err(|e| HandoffError::Activate(e.to_string()))?;

        // Silence the speakers; loopback keeps delivering (pre-mute tap).
        vol.SetMute(true, std::ptr::null())
            .map_err(|e| HandoffError::Activate(e.to_string()))?;

        Ok(Endpoint {
            vol,
            orig_scalar,
            orig_muted,
        })
    }
}

/// The poll thread body: init COM, set up the endpoint, then mirror volume
/// changes until stopped, restoring the original state on exit.
fn run(
    stop: Arc<AtomicBool>,
    ready_tx: Sender<Result<(), HandoffError>>,
    event_tx: Sender<VolumeEvent>,
) {
    unsafe {
        // COM must be initialized on the thread that uses the interfaces.
        if let Err(e) = CoInitializeEx(None, COINIT_MULTITHREADED).ok() {
            let _ = ready_tx.send(Err(HandoffError::ComInit(e.to_string())));
            return;
        }

        let endpoint = match setup_endpoint() {
            Ok(ep) => ep,
            Err(e) => {
                let _ = ready_tx.send(Err(e));
                CoUninitialize();
                return;
            }
        };

        // Setup succeeded; unblock start().
        let _ = ready_tx.send(Ok(()));
        info!("handoff: local speakers muted, mirroring Windows volume");

        let mut state = MirrorState::initial(endpoint.orig_scalar);
        while !stop.load(Ordering::SeqCst) {
            std::thread::sleep(POLL_INTERVAL);

            let cur_scalar = endpoint
                .vol
                .GetMasterVolumeLevelScalar()
                .unwrap_or(state.last_scalar);
            let cur_muted = endpoint
                .vol
                .GetMute()
                .map(|b| b.as_bool())
                .unwrap_or(true);

            let action = classify(&state, cur_scalar, cur_muted);
            if action.reassert_mute {
                let _ = endpoint.vol.SetMute(true, std::ptr::null());
            }
            if let Some(ev) = action.emit {
                if event_tx.send(ev).is_err() {
                    break; // receiver dropped — stream ended
                }
            }
            state = action.next;
        }

        // Restore what the user had before we took over.
        if let Err(e) = endpoint
            .vol
            .SetMasterVolumeLevelScalar(endpoint.orig_scalar, std::ptr::null())
        {
            warn!("handoff: failed to restore volume level: {e}");
        }
        if let Err(e) = endpoint.vol.SetMute(endpoint.orig_muted, std::ptr::null()) {
            warn!("handoff: failed to restore mute state: {e}");
        }
        info!("handoff: restored original speaker volume/mute");
        CoUninitialize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.05
    }

    #[test]
    fn scalar_to_dbfs_endpoints() {
        assert!(approx(scalar_to_dbfs(1.0), 0.0));
        assert!(approx(scalar_to_dbfs(0.5), -6.02));
        // 0.0316 ≈ -30 dB, right at the clamp floor.
        assert!(approx(scalar_to_dbfs(0.0316), -30.0));
        // Anything quieter clamps to the floor, not below.
        assert!(approx(scalar_to_dbfs(0.001), -30.0));
        assert_eq!(scalar_to_dbfs(0.0), MUTE_DBFS);
    }

    #[test]
    fn classify_no_change_is_noop() {
        let s = MirrorState::initial(0.5);
        let a = classify(&s, 0.5, true);
        assert_eq!(a.emit, None);
        assert!(!a.reassert_mute);
        assert_eq!(a.next, s);
    }

    #[test]
    fn classify_volume_raise_with_auto_unmute() {
        // Windows raises the scalar AND clears our mute on a volume-key press.
        let s = MirrorState::initial(0.5);
        let a = classify(&s, 0.6, false);
        assert_eq!(a.emit, Some(VolumeEvent::Level(scalar_to_dbfs(0.6))));
        assert!(a.reassert_mute, "must re-mute after Windows auto-unmuted");
        assert!(!a.next.airplay_muted);
        assert_eq!(a.next.last_scalar, 0.6);
    }

    #[test]
    fn classify_volume_change_while_still_muted() {
        // Scalar changed but endpoint stayed muted → no re-assert needed.
        let s = MirrorState::initial(0.5);
        let a = classify(&s, 0.4, true);
        assert_eq!(a.emit, Some(VolumeEvent::Level(scalar_to_dbfs(0.4))));
        assert!(!a.reassert_mute);
    }

    #[test]
    fn classify_user_mute_toggle_roundtrip() {
        // Start audible. User taps mute → endpoint unmutes (true→false), scalar
        // unchanged. We read it as "mute AirPlay".
        let s = MirrorState::initial(0.5);
        let a1 = classify(&s, 0.5, false);
        assert_eq!(a1.emit, Some(VolumeEvent::Level(MUTE_DBFS)));
        assert!(a1.reassert_mute);
        assert!(a1.next.airplay_muted);

        // Our re-assert makes the next sample read muted=true again: no-op.
        let a_hold = classify(&a1.next, 0.5, true);
        assert_eq!(a_hold.emit, None);

        // User taps mute again → unmute AirPlay, restoring the audible level.
        let a2 = classify(&a1.next, 0.5, false);
        assert_eq!(
            a2.emit,
            Some(VolumeEvent::Level(scalar_to_dbfs(0.5))),
            "second toggle restores the audible level"
        );
        assert!(a2.reassert_mute);
        assert!(!a2.next.airplay_muted);
    }

    #[test]
    fn classify_volume_move_unmutes_airplay() {
        // If AirPlay is muted and the user moves the volume, that unmutes it.
        let mut s = MirrorState::initial(0.5);
        s.airplay_muted = true;
        let a = classify(&s, 0.7, false);
        assert_eq!(a.emit, Some(VolumeEvent::Level(scalar_to_dbfs(0.7))));
        assert!(!a.next.airplay_muted);
    }
}
