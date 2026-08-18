use std::collections::HashMap;
use std::net::IpAddr;

use crate::device::AirPlayDevice;

/// Collates resolved mDNS announcements into one entry per physical device.
///
/// mDNS re-announces constantly and a device can resolve on several addresses,
/// so the same receiver arrives many times. Both the fixed-window `browse` and
/// the interactive picker need the identical collation rules — a device shown
/// twice in a list, or listed on a link-local IPv6 address we then fail to
/// connect to, are the same bug seen from two places. Keeping the rules here
/// means there is only one of them.
#[derive(Debug, Default)]
pub struct DeviceSet {
    by_key: HashMap<String, AirPlayDevice>,
}

impl DeviceSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add or update a device. Returns `true` if the collection actually
    /// changed, so a caller driving a UI can skip a redraw on the (very
    /// common) repeat announcement.
    pub fn insert(&mut self, device: AirPlayDevice) -> bool {
        let key = Self::key_for(&device);
        match self.by_key.get(&key) {
            // Upgrade an existing IPv6 entry when IPv4 shows up. Never the
            // reverse: link-local IPv6 is what we fall back to, not prefer.
            Some(existing) => {
                let upgrade = matches!(existing.addr, IpAddr::V6(_))
                    && matches!(device.addr, IpAddr::V4(_));
                if upgrade {
                    self.by_key.insert(key, device);
                }
                upgrade
            }
            None => {
                self.by_key.insert(key, device);
                true
            }
        }
    }

    /// Identity key: the device ID when the TXT record carries one, else
    /// address+port. Two receivers can share a display name, so the name is
    /// never part of the key.
    ///
    /// Public because a UI needs to track a selection across re-sorts, and
    /// keying that on a list index would silently select the wrong receiver
    /// the moment a new device arrives and shifts the rows.
    pub fn key_for(device: &AirPlayDevice) -> String {
        device
            .txt
            .device_id
            .clone()
            .unwrap_or_else(|| format!("{}:{}", device.addr, device.port))
    }

    pub fn len(&self) -> usize {
        self.by_key.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_key.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &AirPlayDevice> {
        self.by_key.values()
    }

    /// Consume into a name-sorted list, for stable output.
    pub fn into_sorted(self) -> Vec<AirPlayDevice> {
        let mut devices: Vec<AirPlayDevice> = self.by_key.into_values().collect();
        devices.sort_by(|a, b| a.name.cmp(&b.name));
        devices
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::txt::AirPlayTxt;
    use std::collections::HashMap as Map;

    fn device(name: &str, addr: &str, id: Option<&str>) -> AirPlayDevice {
        let mut raw: Map<String, String> = Map::new();
        // Bit 9 (SupportsAirPlayAudio) must be set or the browser filters it
        // out upstream; DeviceSet itself does not care, but keeping test data
        // realistic avoids surprises if that ever moves.
        raw.insert("features".into(), "0x200".into());
        if let Some(id) = id {
            raw.insert("deviceid".into(), id.into());
        }
        let txt = AirPlayTxt::parse(&raw);
        AirPlayDevice::new(name.into(), addr.parse().unwrap(), 7000, txt)
    }

    #[test]
    fn first_insert_reports_a_change() {
        let mut set = DeviceSet::new();
        assert!(set.insert(device("Living Room", "192.168.1.10", Some("AA:BB"))));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn repeat_announcement_is_idempotent() {
        let mut set = DeviceSet::new();
        set.insert(device("Living Room", "192.168.1.10", Some("AA:BB")));
        // mDNS re-announces the same record; the list must not grow and the
        // caller must be told nothing changed.
        assert!(!set.insert(device("Living Room", "192.168.1.10", Some("AA:BB"))));
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn ipv4_upgrades_an_ipv6_entry() {
        let mut set = DeviceSet::new();
        set.insert(device("Living Room", "fe80::1", Some("AA:BB")));
        assert!(set.insert(device("Living Room", "192.168.1.10", Some("AA:BB"))));
        assert_eq!(set.len(), 1);
        assert_eq!(
            set.iter().next().unwrap().addr,
            "192.168.1.10".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn ipv6_never_replaces_ipv4() {
        let mut set = DeviceSet::new();
        set.insert(device("Living Room", "192.168.1.10", Some("AA:BB")));
        assert!(!set.insert(device("Living Room", "fe80::1", Some("AA:BB"))));
        assert_eq!(
            set.iter().next().unwrap().addr,
            "192.168.1.10".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn devices_without_ids_are_keyed_by_address() {
        let mut set = DeviceSet::new();
        set.insert(device("Speaker", "192.168.1.10", None));
        set.insert(device("Speaker", "192.168.1.11", None));
        // Same display name, no device ID — these are two different receivers
        // and must both be listed.
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn into_sorted_orders_by_name() {
        let mut set = DeviceSet::new();
        set.insert(device("Pool Room", "192.168.1.11", Some("2")));
        set.insert(device("Kitchen", "192.168.1.12", Some("3")));
        set.insert(device("Living Room", "192.168.1.10", Some("1")));
        let names: Vec<String> = set.into_sorted().into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["Kitchen", "Living Room", "Pool Room"]);
    }
}
