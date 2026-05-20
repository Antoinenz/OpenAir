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
/// Collects all `ServiceResolved` events for the full timeout window, then
/// emits one device per unique ID — preferring IPv4 over link-local IPv6.
/// Browsing both `_airplay._tcp` and `_raop._tcp`.
pub fn browse(
    timeout: Duration,
    mut on_device: impl FnMut(AirPlayDevice),
) -> Result<(), mdns_sd::Error> {
    let daemon = ServiceDaemon::new()?;

    let airplay_recv = daemon.browse(AIRPLAY_SERVICE)?;
    let raop_recv = daemon.browse(RAOP_SERVICE)?;

    let deadline = std::time::Instant::now() + timeout;
    // key = device_id (or "addr:port") → best device seen so far
    let mut best: HashMap<String, AirPlayDevice> = HashMap::new();

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }

        for event in airplay_recv.try_iter().chain(raop_recv.try_iter()) {
            if let Some(device) = handle_event(event) {
                let key = device
                    .txt
                    .device_id
                    .clone()
                    .unwrap_or_else(|| format!("{}:{}", device.addr, device.port));

                let entry = best.entry(key);
                use std::collections::hash_map::Entry;
                match entry {
                    Entry::Vacant(v) => { v.insert(device); }
                    Entry::Occupied(mut o) => {
                        // Upgrade from IPv6 to IPv4 if a better address arrives.
                        let existing_is_v6 = matches!(o.get().addr, IpAddr::V6(_));
                        let new_is_v4 = matches!(device.addr, IpAddr::V4(_));
                        if existing_is_v6 && new_is_v4 {
                            o.insert(device);
                        }
                    }
                }
            }
        }

        std::thread::sleep(Duration::from_millis(100).min(remaining));
    }

    // Emit all collected devices, sorted by name for stable output.
    let mut devices: Vec<AirPlayDevice> = best.into_values().collect();
    devices.sort_by(|a, b| a.name.cmp(&b.name));
    for device in devices {
        on_device(device);
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
/// mdns-sd may return only one address — the upgrade logic in `browse` handles
/// the cross-event case.
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
