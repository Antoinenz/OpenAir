/// TLV8 encoding/decoding for HomeKit pair-setup messages.
///
/// Format: type (1 byte) || length (1 byte) || value (length bytes).
/// Values longer than 255 bytes are split into consecutive TLVs with the same type.
use std::collections::HashMap;

/// TLV8 type codes used in HomeKit pairing.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tag {
    Method = 0x00,
    Identifier = 0x01,
    Salt = 0x02,
    PublicKey = 0x03,
    Proof = 0x04,
    EncryptedData = 0x05,
    State = 0x06,
    Error = 0x07,
    Signature = 0x0A,
    /// Apple-internal tag (19) — carries pairing flags, e.g. 0x10 = Transient.
    Flags = 0x13,
}

/// Flag value for `Tag::Flags`: kPairingFlag_Transient.
pub const FLAG_TRANSIENT: u8 = 0x10;

impl Tag {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x00 => Some(Tag::Method),
            0x01 => Some(Tag::Identifier),
            0x02 => Some(Tag::Salt),
            0x03 => Some(Tag::PublicKey),
            0x04 => Some(Tag::Proof),
            0x05 => Some(Tag::EncryptedData),
            0x06 => Some(Tag::State),
            0x07 => Some(Tag::Error),
            0x0A => Some(Tag::Signature),
            0x13 => Some(Tag::Flags),
            _ => None,
        }
    }
}

/// Encode a map of tag → value into TLV8 bytes.
/// Values longer than 255 bytes are automatically split.
pub fn encode(items: &[(Tag, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (tag, value) in items {
        let tag_byte = *tag as u8;
        if value.is_empty() {
            out.push(tag_byte);
            out.push(0);
        } else {
            for chunk in value.chunks(255) {
                out.push(tag_byte);
                out.push(chunk.len() as u8);
                out.extend_from_slice(chunk);
            }
        }
    }
    out
}

/// Decode TLV8 bytes into a map of tag → concatenated value.
/// Adjacent TLVs with the same type are concatenated automatically.
pub fn decode(data: &[u8]) -> HashMap<u8, Vec<u8>> {
    let mut map: HashMap<u8, Vec<u8>> = HashMap::new();
    let mut i = 0;
    while i + 1 < data.len() {
        let tag = data[i];
        let len = data[i + 1] as usize;
        i += 2;
        if i + len > data.len() {
            break;
        }
        map.entry(tag).or_default().extend_from_slice(&data[i..i + len]);
        i += len;
    }
    map
}

/// Convenience: decode and get a known tag's value.
pub fn get(decoded: &HashMap<u8, Vec<u8>>, tag: Tag) -> Option<&Vec<u8>> {
    decoded.get(&(tag as u8))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_short() {
        let salt = vec![0xAAu8; 16];
        let pub_key = vec![0xBBu8; 32];
        let encoded = encode(&[
            (Tag::State, &[0x01]),
            (Tag::Salt, &salt),
            (Tag::PublicKey, &pub_key),
        ]);
        let decoded = decode(&encoded);
        assert_eq!(decoded[&(Tag::State as u8)], vec![0x01]);
        assert_eq!(decoded[&(Tag::Salt as u8)], salt);
        assert_eq!(decoded[&(Tag::PublicKey as u8)], pub_key);
    }

    #[test]
    fn long_value_is_split_and_rejoined() {
        // Values >255 bytes must be fragmented and reassembled.
        let big = vec![0xCCu8; 512];
        let encoded = encode(&[(Tag::PublicKey, &big)]);
        // Should have two TLV segments for this tag (255 + 257 bytes)
        assert_eq!(encoded[0], Tag::PublicKey as u8);
        assert_eq!(encoded[1], 255);
        let decoded = decode(&encoded);
        assert_eq!(decoded[&(Tag::PublicKey as u8)], big);
    }

    #[test]
    fn empty_value() {
        let encoded = encode(&[(Tag::Method, &[])]);
        let decoded = decode(&encoded);
        assert_eq!(decoded[&(Tag::Method as u8)], Vec::<u8>::new());
    }
}
