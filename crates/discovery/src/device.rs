use openair_core::types::{AudioMode, Features};
use std::net::IpAddr;

use crate::txt::AirPlayTxt;

/// A discovered AirPlay 2 receiver on the local network.
#[derive(Debug, Clone)]
pub struct AirPlayDevice {
    /// Friendly service name as advertised via mDNS.
    pub name: String,
    /// IP address resolved from the mDNS record.
    pub addr: IpAddr,
    /// TCP port for RTSP control (from SRV record — never hardcode 7000).
    pub port: u16,
    /// Parsed TXT record fields.
    pub txt: AirPlayTxt,
}

impl AirPlayDevice {
    pub fn new(name: String, addr: IpAddr, port: u16, txt: AirPlayTxt) -> Self {
        Self { name, addr, port, txt }
    }

    pub fn features(&self) -> Features {
        self.txt.features
    }

    /// Whether this device is HomePod-class (requires PTP, prefers AAC).
    pub fn is_homepod_class(&self) -> bool {
        self.txt.model
            .as_deref()
            .map(|m| m.starts_with("AudioAccessory"))
            .unwrap_or(false)
    }

    /// Select the audio mode based on feature bits.
    /// Bit 40 → AAC PT=103; otherwise ALAC PT=96.
    pub fn preferred_audio_mode(&self) -> AudioMode {
        if self.txt.features.supports_buffered_audio() {
            AudioMode::BufferedAac
        } else {
            AudioMode::RealtimeAlac
        }
    }

    /// True if this device uses Transient pairing (X-Apple-HKP: 4).
    pub fn uses_transient_pairing(&self) -> bool {
        self.txt.features.supports_transient_pairing()
    }

    /// The receiver's name without mDNS decoration.
    ///
    /// `name` is the raw fullname, which is not what anyone calls their
    /// speaker: `_airplay._tcp` gives `Living Room._airplay._tcp.local.` and
    /// `_raop._tcp` prefixes the device ID as well —
    /// `AABBCCDDEEFF@Living Room._raop._tcp.local.`
    pub fn display_name(&self) -> &str {
        if let Some(name) = self.name.strip_suffix("._airplay._tcp.local.") {
            // No device-id prefix on this service, so an '@' here belongs to
            // the user's chosen name and must be left alone.
            return name;
        }
        if let Some(name) = self.name.strip_suffix("._raop._tcp.local.") {
            // `<deviceid>@<name>`. Device IDs never contain '@', so the first
            // one is the separator — splitting there keeps an '@' that is part
            // of the name.
            return match name.split_once('@') {
                Some((_id, rest)) => rest,
                None => name,
            };
        }
        &self.name
    }
}

impl std::fmt::Display for AirPlayDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} @ {}:{} [{}] audio={:?} ptp={} transient={}",
            self.name,
            self.addr,
            self.port,
            self.txt.model.as_deref().unwrap_or("unknown"),
            self.preferred_audio_mode(),
            self.features().requires_ptp(),
            self.uses_transient_pairing(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str) -> AirPlayDevice {
        AirPlayDevice::new(
            name.into(),
            "192.168.1.10".parse().unwrap(),
            7000,
            AirPlayTxt::default(),
        )
    }

    #[test]
    fn strips_the_airplay_service_suffix() {
        assert_eq!(named("Living Room._airplay._tcp.local.").display_name(), "Living Room");
    }

    #[test]
    fn strips_the_raop_suffix_and_device_id_prefix() {
        assert_eq!(
            named("AABBCCDDEEFF@Pool Room._raop._tcp.local.").display_name(),
            "Pool Room"
        );
    }

    #[test]
    fn leaves_an_undecorated_name_alone() {
        assert_eq!(named("Kitchen").display_name(), "Kitchen");
    }

    #[test]
    fn keeps_an_at_sign_that_belongs_to_the_name() {
        // Only the device-id prefix is stripped; an '@' the user put in the
        // name stays.
        assert_eq!(
            named("AABBCCDDEEFF@Bar@Home._raop._tcp.local.").display_name(),
            "Bar@Home"
        );
        assert_eq!(named("Bar@Home._airplay._tcp.local.").display_name(), "Bar@Home");
    }
}
