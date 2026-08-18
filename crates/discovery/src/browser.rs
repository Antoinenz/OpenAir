use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::Arc;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent};
use tracing::{debug, info};

use crate::device::AirPlayDevice;
use crate::set::DeviceSet;
use crate::txt::AirPlayTxt;

const AIRPLAY_SERVICE: &str = "_airplay._tcp.local.";
const RAOP_SERVICE: &str = "_raop._tcp.local.";

/// How often the browse thread drains the mDNS event queues.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// A browse that runs until dropped.
///
/// Devices arrive on [`BrowseHandle::devices`] as they resolve, so a caller can
/// show a list that fills in while the user reads it instead of blocking for a
/// fixed window. Announcements are *not* de-duplicated here — feed them through
/// a [`DeviceSet`] for that.
pub struct BrowseHandle {
    /// Resolved devices, in arrival order. Expect repeats: mDNS re-announces.
    pub devices: Receiver<AirPlayDevice>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for BrowseHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // Joining takes up to one poll interval. Worth the wait: the
            // daemon is shut down inside the thread, and leaking mDNS sockets
            // for the rest of the process would break a later browse.
            let _ = thread.join();
        }
    }
}

/// Browse for AirPlay 2 receivers until the returned handle is dropped.
///
/// Unlike [`browse`], this never blocks the caller — it owns a background
/// thread that pumps `_airplay._tcp` and `_raop._tcp` and forwards every
/// resolved device down a channel.
pub fn browse_live() -> Result<BrowseHandle, mdns_sd::Error> {
    let daemon = ServiceDaemon::new()?;
    let airplay_recv = daemon.browse(AIRPLAY_SERVICE)?;
    let raop_recv = daemon.browse(RAOP_SERVICE)?;

    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);

    let thread = std::thread::spawn(move || {
        while !thread_stop.load(Ordering::Relaxed) {
            for event in airplay_recv.try_iter().chain(raop_recv.try_iter()) {
                if let Some(device) = handle_event(event) {
                    // A send error means the receiver is gone; nothing left to
                    // report to, so stop early rather than spin.
                    if tx.send(device).is_err() {
                        break;
                    }
                }
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        // mdns-sd v0.11 has a benign race on shutdown — ignore the error.
        let _ = daemon.shutdown();
    });

    Ok(BrowseHandle {
        devices: rx,
        stop,
        thread: Some(thread),
    })
}

/// Browse for AirPlay 2 receivers on the local network for a fixed window.
///
/// Collects announcements for the full timeout, then emits one device per
/// unique ID — preferring IPv4 over link-local IPv6 — sorted by name. Used by
/// the non-interactive paths; the TUI picker drives [`browse_live`] directly so
/// it can render while results arrive.
pub fn browse(
    timeout: Duration,
    mut on_device: impl FnMut(AirPlayDevice),
) -> Result<(), mdns_sd::Error> {
    let handle = browse_live()?;
    let deadline = std::time::Instant::now() + timeout;
    let mut seen = DeviceSet::new();

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        while let Ok(device) = handle.devices.try_recv() {
            seen.insert(device);
        }
        std::thread::sleep(POLL_INTERVAL.min(remaining));
    }

    for device in seen.into_sorted() {
        on_device(device);
    }
    Ok(())
}

fn handle_event(event: ServiceEvent) -> Option<AirPlayDevice> {
    match event {
        ServiceEvent::ServiceResolved(info) => {
            let addr = pick_addr(info.get_addresses())?;
            let port = info.get_port();
            let name = info.get_fullname().to_string();

            let raw_txt: HashMap<String, String> = info
                .get_properties()
                .iter()
                .map(|p| (p.key().to_string(), p.val_str().to_string()))
                .collect();

            let txt = AirPlayTxt::parse(&raw_txt);

            if !txt.features.supports_airplay_audio() {
                debug!(name = %name, "skipping: bit 9 (SupportsAirPlayAudio) not set");
                return None;
            }

            info!(
                name = %name,
                addr = %addr,
                port = port,
                model = ?txt.model,
                ptp = txt.features.requires_ptp(),
                audio = ?if txt.features.supports_buffered_audio() { "AAC" } else { "ALAC" },
                "discovered AirPlay device"
            );

            Some(AirPlayDevice::new(name, addr, port, txt))
        }
        ServiceEvent::ServiceRemoved(_, fullname) => {
            info!(name = %fullname, "AirPlay device removed");
            None
        }
        _ => None,
    }
}

/// Prefer IPv4; fall back to first address. Within a single resolution event,
/// mdns-sd may return only one address — the upgrade logic in [`DeviceSet`]
/// handles the cross-event case.
fn pick_addr<'a>(addrs: impl IntoIterator<Item = &'a IpAddr>) -> Option<IpAddr> {
    let mut fallback: Option<IpAddr> = None;
    for &addr in addrs {
        if matches!(addr, IpAddr::V4(_)) {
            return Some(addr);
        }
        if fallback.is_none() {
            fallback = Some(addr);
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Dropping the handle must stop the thread and shut the daemon down
    /// without panicking or hanging. If it leaked, a second browse in the same
    /// process would contend for the mDNS sockets — so this also proves two
    /// sequential browses work, which is exactly what the picker does when the
    /// user backs out and starts again.
    #[test]
    fn handle_drop_shuts_down_cleanly() {
        // No network in the test environment is not a failure of this code.
        let Ok(first) = browse_live() else {
            return;
        };
        drop(first);
        let second = browse_live().expect("second browse after clean shutdown");
        drop(second);
    }
}
