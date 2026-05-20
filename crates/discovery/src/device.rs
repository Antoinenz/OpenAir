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
