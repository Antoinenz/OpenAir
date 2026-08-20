//! `--handoff` (Windows only): route system audio through a virtual output
//! device so the physical speakers go quiet, and mirror the Windows volume
//! onto AirPlay.
//!
//! # Why routing, not muting
//!
//! v1 silenced the speakers by holding `IAudioEndpointVolume::SetMute(TRUE)`.
//! That is structurally unwinnable: Windows auto-unmutes the endpoint on every
//! volume change and we can only re-mute *after the fact*, which is audible as
//! a glitch on each volume keypress. An event-driven callback would shrink that
//! window but not close it.
//!
//! v2 instead switches the Windows **default output device** to a virtual audio
//! cable (VB-CABLE et al). The speakers stop receiving audio because they are no
//! longer the default endpoint — nothing is muted, so there is no race and no
//! glitch. As a bonus, Windows' per-app output routing (Settings → System →
//! Sound) then gives users split tunneling for free.
//!
//! Because the volume slider now acts on the *virtual* device (which nobody
//! hears), the mute flag is unambiguous again: muted means the user wants
//! silence. So mirroring reduces to "read scalar + mute, emit dBFS" — v1's
//! re-assert/`classify` state machine is gone.
//!
//! # Safety net
//!
//! Leaving the user routed to a silent cable with no obvious cause is a nasty
//! failure mode, so the original device id is persisted to disk on switch and
//! removed on restore. [`pending_restore`] / [`restore_now`] let the CLI repair
//! it after a crash.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread::JoinHandle;
use std::time::Duration;

use thiserror::Error;
use tracing::{info, warn};

// `IUnknown_Vtbl` is unused by name but must be in scope: the #[interface]
// macro references it when building IPolicyConfig's vtable.
#[allow(unused_imports)]
use windows::core::{IUnknown, IUnknown_Vtbl, HRESULT, PCWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::{BOOL, RPC_E_CHANGED_MODE};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eConsole, eMultimedia, eRender, ERole, IMMDeviceEnumerator, MMDeviceEnumerator,
    DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};

/// How often the poll thread samples the Windows master volume/mute.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// dBFS value meaning "silent" (matches RTSP `set_volume`'s mute sentinel).
const MUTE_DBFS: f32 = -144.0;

/// Quietest non-muted AirPlay level we map to; `20·log10(0.0316) ≈ -30`.
const MIN_DBFS: f32 = -30.0;

/// Scalars within this of each other are treated as unchanged (Windows volume
/// scalars are quantized; a real one-step change is ~0.02).
const SCALAR_EPS: f32 = 1e-4;

/// Substrings (lowercase) identifying a virtual audio cable's *render* endpoint
/// — the device Windows plays INTO. VB-CABLE's is confusingly called "CABLE
/// Input" because it is the cable's input; it still shows up as an output
/// device.
///
/// **Order is preference order.** A VB-CABLE install can expose several
/// matching endpoints (e.g. both `CABLE Input (VB-Audio Virtual Cable)` and
/// `CABLE In 16 Ch (VB-Audio Virtual Cable)`), and enumeration order is not
/// stable or meaningful. The canonical stereo endpoints — the ones apps
/// actually get routed to — come first so we don't land on a multi-channel
/// variant by accident.
const VIRTUAL_CABLE_PATTERNS: &[&str] = &[
    "cable input",
    "voicemeeter input",
    "voicemeeter aux input",
    "virtual audio cable",
    "virtual cable",
    "vb-audio",
];

/// `CPolicyConfigClient` — the undocumented COM class behind default-device
/// switching (what nircmd / SoundVolumeView / AudioSwitcher all use).
const CLSID_POLICY_CONFIG_CLIENT: windows::core::GUID =
    windows::core::GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

/// Undocumented `IPolicyConfig`. There is no public API for changing the
/// default audio endpoint, so the interface is declared by hand.
///
/// Only `SetDefaultEndpoint` is used; the preceding methods exist purely to
/// place it at the correct vtable slot (index 13, after `IUnknown`'s three).
/// Their signatures are deliberately loose (`*mut c_void`) since they are never
/// called — only their *position* matters.
// Scoped in its own module so the non-snake-case allowance (COM method names
// must match the vtable, not Rust style) doesn't leak into our own code.
#[allow(non_snake_case)]
mod policy_config {
    use super::{BOOL, ERole, HRESULT, IUnknown, IUnknown_Vtbl, PCWSTR};

    #[windows::core::interface("f8679f50-850a-41cf-9c72-430f290290c8")]
    pub(super) unsafe trait IPolicyConfig: IUnknown {
        unsafe fn GetMixFormat(&self, name: PCWSTR, fmt: *mut *mut core::ffi::c_void) -> HRESULT;
        unsafe fn GetDeviceFormat(
            &self,
            name: PCWSTR,
            default: BOOL,
            fmt: *mut *mut core::ffi::c_void,
        ) -> HRESULT;
        unsafe fn ResetDeviceFormat(&self, name: PCWSTR) -> HRESULT;
        unsafe fn SetDeviceFormat(
            &self,
            name: PCWSTR,
            endpoint_fmt: *mut core::ffi::c_void,
            mix_fmt: *mut core::ffi::c_void,
        ) -> HRESULT;
        unsafe fn GetProcessingPeriod(
            &self,
            name: PCWSTR,
            default: BOOL,
            def_period: *mut i64,
            min_period: *mut i64,
        ) -> HRESULT;
        unsafe fn SetProcessingPeriod(&self, name: PCWSTR, period: *mut i64) -> HRESULT;
        unsafe fn GetShareMode(&self, name: PCWSTR, mode: *mut core::ffi::c_void) -> HRESULT;
        unsafe fn SetShareMode(&self, name: PCWSTR, mode: *mut core::ffi::c_void) -> HRESULT;
        unsafe fn GetPropertyValue(
            &self,
            name: PCWSTR,
            key: *const core::ffi::c_void,
            value: *mut core::ffi::c_void,
        ) -> HRESULT;
        unsafe fn SetPropertyValue(
            &self,
            name: PCWSTR,
            key: *const core::ffi::c_void,
            value: *mut core::ffi::c_void,
        ) -> HRESULT;
        pub(super) unsafe fn SetDefaultEndpoint(&self, device_id: PCWSTR, role: ERole) -> HRESULT;
        unsafe fn SetEndpointVisibility(&self, device_id: PCWSTR, visible: BOOL) -> HRESULT;
    }
}

use policy_config::IPolicyConfig;

/// A mirrored-volume update to forward to the AirPlay receivers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VolumeEvent {
    /// New AirPlay level in dBFS. `-144` means silent.
    Level(f32),
}

#[derive(Debug, Error)]
pub enum HandoffError {
    #[error("COM initialization failed: {0}")]
    ComInit(String),
    #[error("could not enumerate audio devices: {0}")]
    Enumerate(String),
    #[error(
        "no virtual audio output device found.\n\
         --handoff needs one to route audio around your speakers.\n\
         Install VB-CABLE (free): https://vb-audio.com/Cable/\n\
         Then re-run, or pass --handoff-device \"<name>\" if yours is named differently."
    )]
    NoVirtualDevice,
    #[error("no output device matching '{0}' was found")]
    DeviceNotFound(String),
    #[error("could not switch the default audio device: {0}")]
    SwitchFailed(String),
    #[error("could not access endpoint volume: {0}")]
    Activate(String),
}

/// One Windows audio output endpoint.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioDevice {
    /// Endpoint id, e.g. `{0.0.0.00000000}.{guid}` — stable across reboots.
    pub id: String,
    pub name: String,
}

/// What `openair devices` shows: every output device, plus which one
/// `--handoff` would actually route through.
pub struct DeviceListing {
    pub devices: Vec<AudioDevice>,
    /// Endpoint id `--handoff` would auto-select, if any device qualifies.
    pub selected: Option<String>,
}

/// List output devices and report which one `--handoff` would pick. Read-only
/// — nothing is switched, so users can check detection before committing.
pub fn list_output_devices() -> Result<DeviceListing, HandoffError> {
    let _com = ComGuard::new()?;
    enumerate_render_devices().map(|devices| {
        let selected = select_device(&devices, None).ok().map(|d| d.id.clone());
        DeviceListing { devices, selected }
    })
}

/// Initialises COM for the current thread, and uninitialises on drop —
/// but only if this guard is what initialised it.
///
/// `RPC_E_CHANGED_MODE` means COM is already up on this thread under a
/// different apartment model. That is not a failure: cpal initialises the main
/// thread as an STA before we ever get here, and the calls in this module work
/// fine in either apartment. Treating it as an error is what made `--handoff`
/// report "unavailable" in a process that had already started audio capture,
/// while `openair devices` — a fresh process — saw the cable perfectly well.
///
/// Uninitialising a thread we did not initialise would be worse still: it
/// would tear COM out from under cpal.
struct ComGuard {
    owned: bool,
}

impl ComGuard {
    fn new() -> Result<Self, HandoffError> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            return Ok(Self { owned: true });
        }
        if hr == RPC_E_CHANGED_MODE {
            return Ok(Self { owned: false });
        }
        Err(HandoffError::ComInit(format!("{hr:?}")))
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.owned {
            unsafe { CoUninitialize() };
        }
    }
}

/// Index of the first (most-preferred) pattern this device name matches, or
/// `None` if it isn't a virtual cable. Lower = better candidate.
fn cable_rank(name: &str) -> Option<usize> {
    let lower = name.to_lowercase();
    VIRTUAL_CABLE_PATTERNS.iter().position(|p| lower.contains(p))
}


/// Pick the device to route through: the first name containing `override_name`
/// (case-insensitive) if given, otherwise the first virtual cable found.
fn select_device<'a>(
    devices: &'a [AudioDevice],
    override_name: Option<&str>,
) -> Result<&'a AudioDevice, HandoffError> {
    match override_name {
        Some(want) => {
            let needle = want.to_lowercase();
            devices
                .iter()
                .find(|d| d.name.to_lowercase().contains(&needle))
                .ok_or_else(|| HandoffError::DeviceNotFound(want.to_string()))
        }
        // Best-ranked candidate, not merely the first one enumerated.
        None => devices
            .iter()
            .filter_map(|d| cable_rank(&d.name).map(|r| (r, d)))
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, d)| d)
            .ok_or(HandoffError::NoVirtualDevice),
    }
}

/// Converts a Windows master-volume scalar (`0.0..=1.0`) to AirPlay dBFS,
/// clamped to `[-30, 0]`; `0.0` maps to `-144` (silent).
fn scalar_to_dbfs(scalar: f32) -> f32 {
    if scalar <= 0.0 {
        return MUTE_DBFS;
    }
    (20.0 * scalar.log10()).clamp(MIN_DBFS, 0.0)
}

/// The AirPlay level for a given `(scalar, muted)` sample. Unlike v1 this needs
/// no history: we never touch the mute flag, so it means exactly what it says.
fn level_for(scalar: f32, muted: bool) -> f32 {
    if muted {
        MUTE_DBFS
    } else {
        scalar_to_dbfs(scalar)
    }
}

/// Enumerate active render (output) endpoints with their ids and friendly
/// names. Caller must have initialized COM on this thread.
fn enumerate_render_devices() -> Result<Vec<AudioDevice>, HandoffError> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| HandoffError::ComInit(e.to_string()))?;
        let collection = enumerator
            .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
            .map_err(|e| HandoffError::Enumerate(e.to_string()))?;
        let count = collection
            .GetCount()
            .map_err(|e| HandoffError::Enumerate(e.to_string()))?;

        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count {
            let Ok(device) = collection.Item(i) else {
                continue;
            };
            let Ok(id_ptr) = device.GetId() else { continue };
            let id = id_ptr.to_string().unwrap_or_default();
            CoTaskMemFree(Some(id_ptr.0 as *const core::ffi::c_void));

            let name = device
                .OpenPropertyStore(STGM_READ)
                .ok()
                .and_then(|props| props.GetValue(&PKEY_Device_FriendlyName).ok())
                .map(|v| v.to_string())
                .unwrap_or_default();

            if !id.is_empty() {
                out.push(AudioDevice { id, name });
            }
        }
        Ok(out)
    }
}

/// Current default render endpoint id (the device we must restore later).
fn default_render_id() -> Result<String, HandoffError> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| HandoffError::ComInit(e.to_string()))?;
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| HandoffError::Enumerate(e.to_string()))?;
        let id_ptr = device
            .GetId()
            .map_err(|e| HandoffError::Enumerate(e.to_string()))?;
        let id = id_ptr.to_string().unwrap_or_default();
        CoTaskMemFree(Some(id_ptr.0 as *const core::ffi::c_void));
        Ok(id)
    }
}

/// Make `device_id` the default output for the roles that carry media.
///
/// `eCommunications` is deliberately left alone so voice/call apps keep using
/// the user's headset instead of being routed to AirPlay.
fn set_default_endpoint(device_id: &str) -> Result<(), HandoffError> {
    unsafe {
        let config: IPolicyConfig = CoCreateInstance(&CLSID_POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)
            .map_err(|e| HandoffError::SwitchFailed(e.to_string()))?;
        let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
        for role in [eConsole, eMultimedia] {
            config
                .SetDefaultEndpoint(PCWSTR(wide.as_ptr()), role)
                .ok()
                .map_err(|e| HandoffError::SwitchFailed(e.to_string()))?;
        }
        Ok(())
    }
}

/// Endpoint volume interface for a specific device id.
fn endpoint_volume_for(device_id: &str) -> Result<IAudioEndpointVolume, HandoffError> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| HandoffError::ComInit(e.to_string()))?;
        let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();
        let device = enumerator
            .GetDevice(PCWSTR(wide.as_ptr()))
            .map_err(|e| HandoffError::Activate(e.to_string()))?;
        device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| HandoffError::Activate(e.to_string()))
    }
}

// --- crash-recovery persistence -----------------------------------------

/// Where the pre-handoff device id is stashed while a session is active.
fn restore_file() -> Option<PathBuf> {
    openair_core::config::config_file("handoff_restore.txt")
}

fn persist_restore(device_id: &str) {
    let Some(path) = restore_file() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, device_id) {
        warn!("handoff: could not persist restore point: {e}");
    }
}

fn clear_restore() {
    if let Some(path) = restore_file() {
        let _ = std::fs::remove_file(path);
    }
}

/// The device id a previous run switched away from but never restored (i.e. it
/// crashed), if any. `None` in the normal case.
pub fn pending_restore() -> Option<String> {
    let path = restore_file()?;
    let id = std::fs::read_to_string(path).ok()?;
    let id = id.trim().to_string();
    (!id.is_empty()).then_some(id)
}

/// Restore the default output device recorded by an interrupted session.
/// Returns the friendly name of the device restored to.
pub fn restore_now() -> Result<String, HandoffError> {
    let id = pending_restore().ok_or(HandoffError::NoVirtualDevice)?;
    let _com = ComGuard::new()?;
    let result = (|| {
        set_default_endpoint(&id)?;
        let name = enumerate_render_devices()?
            .into_iter()
            .find(|d| d.id == id)
            .map(|d| d.name)
            .unwrap_or_else(|| id.clone());
        Ok(name)
    })();
    if result.is_ok() {
        clear_restore();
    }
    unsafe { CoUninitialize() };
    result
}

// --- session -------------------------------------------------------------

/// An active handoff: the default output device has been switched to a virtual
/// cable and the Windows volume is being mirrored. Dropping this restores the
/// original device.
pub struct HandoffSession {
    device_name: String,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

/// What the worker reports back once routing is established.
struct Started {
    device_name: String,
}

impl HandoffSession {
    /// Route system audio to a virtual output device and start mirroring the
    /// Windows volume. `device_override` forces a specific device by name
    /// substring; `None` auto-detects a virtual cable.
    ///
    /// Returns the session (keep it alive for the stream's lifetime — dropping
    /// it restores the original device) and a channel of [`VolumeEvent`]s.
    pub fn start(
        device_override: Option<String>,
    ) -> Result<(HandoffSession, Receiver<VolumeEvent>), HandoffError> {
        let (event_tx, event_rx) = std::sync::mpsc::channel::<VolumeEvent>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<Started, HandoffError>>();
        let stop = Arc::new(AtomicBool::new(false));

        let thread_stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name("handoff-routing".into())
            .spawn(move || run(thread_stop, device_override, ready_tx, event_tx))
            .expect("spawn handoff thread");

        // Block until routing is established so a failure surfaces as a clean
        // error instead of a half-started session.
        match ready_rx.recv() {
            Ok(Ok(started)) => Ok((
                HandoffSession {
                    device_name: started.device_name,
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
                Err(HandoffError::ComInit("handoff thread exited early".into()))
            }
        }
    }

    /// Friendly name of the virtual device audio is being routed through.
    pub fn device_name(&self) -> &str {
        &self.device_name
    }
}

impl Drop for HandoffSession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Worker thread: owns every COM object (they have apartment affinity), so
/// setup, polling, and the restore all happen here.
fn run(
    stop: Arc<AtomicBool>,
    device_override: Option<String>,
    ready_tx: Sender<Result<Started, HandoffError>>,
    event_tx: Sender<VolumeEvent>,
) {
    let _com = match ComGuard::new() {
        Ok(guard) => guard,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return;
        }
    };

    let setup = (|| -> Result<(String, String, IAudioEndpointVolume), HandoffError> {
        let devices = enumerate_render_devices()?;
        let target = select_device(&devices, device_override.as_deref())?;
        let target_id = target.id.clone();
        let target_name = target.name.clone();

        let original_id = default_render_id()?;
        // Persist BEFORE switching: if we die mid-switch the user can recover.
        persist_restore(&original_id);
        set_default_endpoint(&target_id)?;

        let vol = endpoint_volume_for(&target_id)?;
        Ok((original_id, target_name, vol))
    })();

    let (original_id, device_name, vol) = match setup {
        Ok(v) => v,
        Err(e) => {
            clear_restore();
            let _ = ready_tx.send(Err(e));
            unsafe { CoUninitialize() };
            return;
        }
    };

    info!(device = %device_name, "handoff: system audio routed to virtual device");
    let _ = ready_tx.send(Ok(Started {
        device_name: device_name.clone(),
    }));

    // --- mirror loop ---
    let mut last_scalar = f32::NAN;
    let mut last_muted = None::<bool>;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(POLL_INTERVAL);

        let scalar = unsafe { vol.GetMasterVolumeLevelScalar() }.unwrap_or(last_scalar);
        let muted = unsafe { vol.GetMute() }.map(|b| b.as_bool()).unwrap_or(false);

        let scalar_changed = !last_scalar.is_finite() || (scalar - last_scalar).abs() > SCALAR_EPS;
        if scalar_changed || Some(muted) != last_muted {
            if event_tx.send(VolumeEvent::Level(level_for(scalar, muted))).is_err() {
                break; // receiver dropped — stream ended
            }
            last_scalar = scalar;
            last_muted = Some(muted);
        }
    }

    // Put the user's audio back where they left it.
    match set_default_endpoint(&original_id) {
        Ok(()) => {
            clear_restore();
            info!("handoff: restored original default output device");
        }
        Err(e) => warn!("handoff: FAILED to restore default output device: {e}"),
    }
    unsafe { CoUninitialize() };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(id: &str, name: &str) -> AudioDevice {
        AudioDevice {
            id: id.to_string(),
            name: name.to_string(),
        }
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.05
    }

    #[test]
    fn detects_common_virtual_cables() {
        assert!(cable_rank("CABLE Input (VB-Audio Virtual Cable)").is_some());
        assert!(cable_rank("VoiceMeeter Input (VB-Audio VoiceMeeter VAIO)").is_some());
        assert!(cable_rank("Line 1 (Virtual Audio Cable)").is_some());
    }

    #[test]
    fn does_not_flag_real_devices() {
        assert!(cable_rank("Speakers (Realtek(R) Audio)").is_none());
        assert!(cable_rank("Headphones (2- USB Audio Device)").is_none());
        assert!(cable_rank("Denon AVR-X2700H").is_none());
        assert!(cable_rank("Surface Omnisonic Speakers (Surface High Definition Audio)").is_none());
    }

    #[test]
    fn select_device_auto_picks_the_cable_not_the_speakers() {
        let devices = vec![
            dev("{0}.{a}", "Speakers (Realtek(R) Audio)"),
            dev("{0}.{b}", "CABLE Input (VB-Audio Virtual Cable)"),
        ];
        let picked = select_device(&devices, None).expect("should find cable");
        assert_eq!(picked.id, "{0}.{b}");
    }

    /// Real device list from a VB-CABLE install: several endpoints match, and
    /// the 16-channel variant enumerates FIRST. We must still pick the
    /// canonical stereo "CABLE Input" that apps get routed to.
    #[test]
    fn select_device_prefers_canonical_cable_over_multichannel_variant() {
        let devices = vec![
            dev("{0}.{a}", "CABLE In 16 Ch (VB-Audio Virtual Cable)"),
            dev("{0}.{b}", "Outputs (Omnibus)"),
            dev("{0}.{c}", "CABLE Input (VB-Audio Virtual Cable)"),
            dev("{0}.{d}", "Surface Omnisonic Speakers (Surface High Definition Audio)"),
        ];
        let picked = select_device(&devices, None).expect("should find cable");
        assert_eq!(
            picked.name, "CABLE Input (VB-Audio Virtual Cable)",
            "must prefer the stereo endpoint over the 16-channel one"
        );
    }

    #[test]
    fn select_device_errors_when_no_cable_installed() {
        let devices = vec![dev("{0}.{a}", "Speakers (Realtek(R) Audio)")];
        assert!(matches!(
            select_device(&devices, None),
            Err(HandoffError::NoVirtualDevice)
        ));
    }

    #[test]
    fn select_device_override_wins_and_is_case_insensitive() {
        let devices = vec![
            dev("{0}.{a}", "Speakers (Realtek(R) Audio)"),
            dev("{0}.{b}", "CABLE Input (VB-Audio Virtual Cable)"),
        ];
        // An override can select a device our patterns would never match.
        let picked = select_device(&devices, Some("realtek")).unwrap();
        assert_eq!(picked.id, "{0}.{a}");
    }

    #[test]
    fn select_device_override_missing_is_an_error() {
        let devices = vec![dev("{0}.{a}", "Speakers (Realtek(R) Audio)")];
        assert!(matches!(
            select_device(&devices, Some("nonexistent")),
            Err(HandoffError::DeviceNotFound(_))
        ));
    }

    #[test]
    fn scalar_to_dbfs_endpoints() {
        assert!(approx(scalar_to_dbfs(1.0), 0.0));
        assert!(approx(scalar_to_dbfs(0.5), -6.02));
        assert!(approx(scalar_to_dbfs(0.0316), -30.0));
        // Anything quieter clamps to the floor rather than going below it.
        assert!(approx(scalar_to_dbfs(0.001), -30.0));
        assert_eq!(scalar_to_dbfs(0.0), MUTE_DBFS);
    }

    #[test]
    fn level_for_respects_mute_regardless_of_scalar() {
        assert_eq!(level_for(1.0, true), MUTE_DBFS);
        assert_eq!(level_for(0.0, false), MUTE_DBFS);
        assert!(approx(level_for(1.0, false), 0.0));
    }
}
