use openair_core::types::Features;
use std::collections::HashMap;

/// Parsed TXT record from an `_airplay._tcp` or `_raop._tcp` mDNS service entry.
#[derive(Debug, Clone, Default)]
pub struct AirPlayTxt {
    pub device_id: Option<String>,
    pub model: Option<String>,
    pub src_vers: Option<String>,
    pub features: Features,
    /// Raw `sf` / `flags` value — device state bits (PIN required, password, etc.).
    pub status_flags: u32,
    /// Base64-encoded device public key (used in Normal pairing).
    pub pk: Option<String>,
    pub pi: Option<String>,
    pub psi: Option<String>,
    /// Multi-room group IDs.
    pub gid: Option<String>,
    pub igl: Option<String>,
}

impl AirPlayTxt {
    /// Parse a flat key=value map (as returned by mdns-sd) into a typed struct.
    pub fn parse(map: &HashMap<String, String>) -> Self {
        let mut txt = AirPlayTxt::default();

        txt.device_id = map.get("deviceid").cloned();
        txt.model = map.get("model").cloned();
        txt.src_vers = map.get("srcvers").cloned();
        txt.pk = map.get("pk").cloned();
        txt.pi = map.get("pi").cloned();
        txt.psi = map.get("psi").cloned();
        txt.gid = map.get("gid").cloned();
        txt.igl = map.get("igl").cloned();

        if let Some(f) = map.get("features") {
            txt.features = parse_features(f);
        }

        if let Some(sf) = map.get("sf").or_else(|| map.get("flags")) {
            txt.status_flags = parse_hex_or_dec(sf);
        }

        txt
    }
}

/// Parse the `features` TXT value.
///
/// Format: `0xLOWER,0xUPPER` (two hex u32s) or a plain decimal/hex u64.
fn parse_features(s: &str) -> Features {
    let s = s.trim();
    if let Some((lo_str, hi_str)) = s.split_once(',') {
        let lo = parse_hex_u32(lo_str.trim()) as u64;
        let hi = parse_hex_u32(hi_str.trim()) as u64;
        Features((hi << 32) | lo)
    } else if s.starts_with("0x") || s.starts_with("0X") {
        Features(u64::from_str_radix(&s[2..], 16).unwrap_or(0))
    } else {
        Features(s.parse::<u64>().unwrap_or(0))
    }
}

fn parse_hex_u32(s: &str) -> u32 {
    let s = s.trim();
    if s.starts_with("0x") || s.starts_with("0X") {
        u32::from_str_radix(&s[2..], 16).unwrap_or(0)
    } else {
        s.parse::<u32>().unwrap_or(0)
    }
}

fn parse_hex_or_dec(s: &str) -> u32 {
    let s = s.trim();
    if s.starts_with("0x") || s.starts_with("0X") {
        u32::from_str_radix(&s[2..], 16).unwrap_or(0)
    } else {
        s.parse::<u32>().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_comma_format() {
        // HomePod signature from the research brief: 0x4A7FCA00,0x3C356BD0
        let f = parse_features("0x4A7FCA00,0x3C356BD0");
        assert_eq!(f.0, 0x3C356BD0_4A7FCA00u64);
        assert!(f.supports_airplay_audio());   // bit 9
        assert!(f.supports_buffered_audio());  // bit 40
        assert!(f.requires_ptp());             // bit 41
    }

    #[test]
    fn features_plain_hex() {
        let f = parse_features("0x5A7FDFD5");
        assert_eq!(f.0, 0x5A7FDFD5);
    }

    #[test]
    fn features_decimal() {
        let f = parse_features("512");
        assert_eq!(f.0, 512);
    }

    #[test]
    fn full_txt_parse() {
        let mut map = HashMap::new();
        map.insert("deviceid".into(), "AA:BB:CC:DD:EE:FF".into());
        map.insert("model".into(), "AudioAccessory1,1".into());
        map.insert("srcvers".into(), "420.51.1".into());
        map.insert("features".into(), "0x4A7FCA00,0x3C356BD0".into());
        map.insert("sf".into(), "0x4".into());

        let txt = AirPlayTxt::parse(&map);
        assert_eq!(txt.device_id.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(txt.model.as_deref(), Some("AudioAccessory1,1"));
        assert!(txt.features.requires_ptp());
        assert_eq!(txt.status_flags, 4);
    }
}
