//! DMAP (DAAP) encoding for now-playing metadata.
//!
//! Wire format is a flat TLV: 4-byte ASCII tag, 4-byte big-endian payload
//! length, then the payload. Containers use the same shape and nest their
//! children as the payload — so an `mlit` container holding two fields is
//! just the concatenation of those fields, prefixed by `mlit` and their
//! combined length.

/// One DMAP item: tag, big-endian length, payload.
fn item(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(tag);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Encode a now-playing bundle as an `mlit` ("listing item") container
/// holding `minm` (title), `asar` (artist) and `asal` (album).
///
/// Empty fields are omitted rather than sent blank, so a receiver shows a
/// missing album as absent instead of an empty line.
pub fn encode_now_playing(title: &str, artist: &str, album: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for (tag, value) in [(b"minm", title), (b"asar", artist), (b"asal", album)] {
        if !value.is_empty() {
            body.extend_from_slice(&item(tag, value.as_bytes()));
        }
    }
    item(b"mlit", &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read one item at `pos`, returning (tag, payload, next_pos).
    fn read_item(buf: &[u8], pos: usize) -> ([u8; 4], &[u8], usize) {
        let tag: [u8; 4] = buf[pos..pos + 4].try_into().unwrap();
        let len = u32::from_be_bytes(buf[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let start = pos + 8;
        (tag, &buf[start..start + len], start + len)
    }

    #[test]
    fn item_frames_tag_length_and_payload() {
        let out = item(b"minm", b"hi");
        assert_eq!(&out[0..4], b"minm");
        assert_eq!(&out[4..8], &2u32.to_be_bytes());
        assert_eq!(&out[8..], b"hi");
    }

    #[test]
    fn length_is_big_endian() {
        // 256 bytes must encode as 00 00 01 00, not little-endian 00 01 00 00.
        let out = item(b"minm", &vec![b'x'; 256]);
        assert_eq!(&out[4..8], &[0x00, 0x00, 0x01, 0x00]);
    }

    #[test]
    fn encodes_all_three_fields_in_order() {
        let buf = encode_now_playing("Song", "Artist", "Album");
        let (tag, body, end) = read_item(&buf, 0);
        assert_eq!(&tag, b"mlit");
        assert_eq!(end, buf.len(), "container length must cover the whole body");

        let (t1, v1, p) = read_item(body, 0);
        assert_eq!((&t1, v1), (b"minm", b"Song".as_slice()));
        let (t2, v2, p) = read_item(body, p);
        assert_eq!((&t2, v2), (b"asar", b"Artist".as_slice()));
        let (t3, v3, p) = read_item(body, p);
        assert_eq!((&t3, v3), (b"asal", b"Album".as_slice()));
        assert_eq!(p, body.len());
    }

    #[test]
    fn omits_empty_fields() {
        let buf = encode_now_playing("Song", "", "");
        let (_, body, _) = read_item(&buf, 0);
        let (tag, value, next) = read_item(body, 0);
        assert_eq!(&tag, b"minm");
        assert_eq!(value, b"Song");
        assert_eq!(next, body.len(), "no empty artist/album items");
    }

    #[test]
    fn utf8_payloads_use_byte_length_not_char_count() {
        // "é" is 2 bytes, "日本" is 6 — a char-count length would corrupt the stream.
        let buf = encode_now_playing("café", "日本", "");
        let (_, body, _) = read_item(&buf, 0);
        let (_, title, p) = read_item(body, 0);
        assert_eq!(title, "café".as_bytes());
        assert_eq!(title.len(), 5);
        let (_, artist, _) = read_item(body, p);
        assert_eq!(artist, "日本".as_bytes());
        assert_eq!(artist.len(), 6);
    }

    #[test]
    fn all_empty_produces_an_empty_container() {
        let buf = encode_now_playing("", "", "");
        let (tag, body, end) = read_item(&buf, 0);
        assert_eq!(&tag, b"mlit");
        assert!(body.is_empty());
        assert_eq!(end, buf.len());
    }
}
