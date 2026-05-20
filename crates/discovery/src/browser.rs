use std::collections::HashMap;
use std::net::IpAddr;
use std::time::Duration;

use mdns_sd::{ServiceDaemon, ServiceEvent};
use tracing::{debug, info};

use crate::device::AirPlayDevice;
use crate::txt::AirPlayTxt;

const AIRPLAY_SERVICE: &str = "_airplay._tcp.local.";
const RAOP_SERVICE: &str = "_raop._tcp.local.";

/// Browse for AirPlay 2 receivers on the local network.
///
/// Calls `on_device` for each device found during the browse window.
/// Browses both `_airplay._tcp` and `_raop._tcp` — deduplicates by device ID.
pub fn browse(
    timeout: Duration,
    mut on_device: impl FnMut(AirPlayDevice),
) -> Result<(), mdns_sd::Error> {
    let daemon = ServiceDaemon::new()?;

    let airplay_recv = daemon.browse(AIRPLAY_SERVICE)?;
    let raop_recv = daemon.browse(RAOP_SERVICE)?;

    let deadline = std::time::Instant::now() + timeout;
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        // Drain both channels with a short poll interval.
        for event in airplay_recv.try_iter().chain(raop_recv.try_iter()) {
            if let Some(device) = handle_event(event) {
                let key = device
                    .txt
                    .device_id
                    .clone()
                    .unwrap_or_else(|| format!("{}:{}", device.addr, device.port));
                if seen.insert(key) {
                    on_device(device);
                }
            }
        }

        std::thread::sleep(Duration::from_millis(100).min(remaining));
    }

    // mdns-sd v0.11 has a benign race on shutdown — ignore the error.
    let _ = daemon.shutdown();
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
                .filter_map(|p| {
                    let val = p.val_str().to_string();
                    Some((p.key().to_string(), val))
                })
                .collect();

            let txt = AirPlayTxt::parse(&raw_txt);

            if !txt.features.supports_airplay_audio() {
                // Bit 9 not set — not a valid AirPlay audio receiver.
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

/// Prefer an IPv4 address; fall back to the first address available.
/// Link-local IPv6 (fe80::) requires a scope ID to connect, making it
/// unsuitable as a primary address for TCP connections.
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
