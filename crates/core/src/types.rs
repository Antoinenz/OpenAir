/// 64-bit AirPlay feature bitmask, transmitted as `0xLOWER,0xUPPER` in TXT records.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Features(pub u64);

impl Features {
    pub fn has(&self, bit: u8) -> bool {
        self.0 & (1u64 << bit) != 0
    }

    /// Bit 40 — receiver prefers buffered AAC (PT=103) over realtime ALAC (PT=96).
    pub fn supports_buffered_audio(&self) -> bool { self.has(40) }
    /// Bit 41 — PTP required (multi-room / HomePod).
    pub fn requires_ptp(&self) -> bool { self.has(41) }
    /// Bit 43 or 48 — use Transient pairing (X-Apple-HKP: 4).
    pub fn supports_transient_pairing(&self) -> bool { self.has(43) || self.has(48) }
    /// Bit 9 — receiver supports AirPlay audio at all.
    pub fn supports_airplay_audio(&self) -> bool { self.has(9) }
    /// Bit 26 — MFi auth / auth-setup required (Sonos, newer AirPort Express).
    pub fn needs_auth_setup(&self) -> bool { self.has(26) }
}

/// Audio codec / payload type selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioMode {
    /// PT=96, realtime ALAC, ~2s latency.
    RealtimeAlac,
    /// PT=103, buffered AAC-LC, ~500ms latency.
    BufferedAac,
}

impl AudioMode {
    pub fn rtp_payload_type(&self) -> u8 {
        match self {
            AudioMode::RealtimeAlac => 96,
            AudioMode::BufferedAac => 103,
        }
    }
}
