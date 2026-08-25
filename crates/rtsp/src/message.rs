//! Finding the edges of an RTSP message in a byte stream.
//!
//! Both directions of an AirPlay session need this, and for the same reason:
//! **an RTSP message is not a frame.** The encrypted channel carries at most
//! 1024 bytes of plaintext per frame (see `openair_crypto::MAX_FRAME_PLAINTEXT`
//! — hardware-verified, an Apple TV drops the connection on anything larger),
//! so anything longer arrives in pieces and has to be put back together before
//! it means anything.
//!
//! Shared rather than written twice because the two readers that need it —
//! [`crate::connection`] for responses and the client's reverse event channel
//! for requests — got different answers to the same question once already: the
//! event channel reassembled and the response reader did not.

/// End of the header block (index just past the blank line), if present.
pub fn header_block_end(msg: &[u8]) -> Option<usize> {
    msg.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// Value of a header, case-insensitively, from a header block.
pub fn header_value<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    let want = name.to_ascii_lowercase();
    headers.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        (k.trim().to_ascii_lowercase() == want).then(|| v.trim())
    })
}

/// Body length this message declares, or `None` if it declares none.
///
/// Distinct from "declares zero": a message with no `Content-Length` at all
/// leaves us nothing to wait for, and the caller has to decide what that
/// means rather than being told the body is empty.
pub fn declared_body_len(headers: &[u8]) -> Option<usize> {
    let headers = String::from_utf8_lossy(headers);
    header_value(&headers, "Content-Length").and_then(|v| v.parse::<usize>().ok())
}

/// Total length of the RTSP message at the front of `msg`, once fully arrived.
///
/// `None` while it is still incomplete. A message with no `Content-Length` is
/// treated as ending at its header block, which is what a request without a
/// body looks like.
pub fn message_len(msg: &[u8]) -> Option<usize> {
    let head = header_block_end(msg)?;
    let body_len = declared_body_len(&msg[..head]).unwrap_or(0);
    (msg.len() >= head + body_len).then_some(head + body_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(cseq: u32) -> Vec<u8> {
        let body = b"body-bytes-here";
        format!(
            "POST /command RTSP/1.0\r\nCSeq: {cseq}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes()
        .into_iter()
        .chain(body.iter().copied())
        .collect()
    }

    #[test]
    fn a_message_is_incomplete_until_its_whole_body_has_arrived() {
        let full = sample(5);
        let head_only = header_block_end(&full).unwrap();
        assert_eq!(message_len(&full[..head_only]), None);
        assert_eq!(message_len(&full[..full.len() - 1]), None);
        assert_eq!(message_len(&full), Some(full.len()));
    }

    #[test]
    fn nothing_is_claimed_until_the_headers_are_complete() {
        assert_eq!(message_len(b"POST /command RTSP/1.0\r\nCSeq: 1"), None);
    }

    #[test]
    fn a_bodyless_message_ends_at_its_header_block() {
        let msg = b"RTSP/1.0 200 OK\r\nCSeq: 3\r\nContent-Length: 0\r\n\r\n";
        assert_eq!(message_len(msg), Some(msg.len()));
    }

    #[test]
    fn a_missing_content_length_is_not_a_zero_length_body() {
        // The caller has to tell these apart: "the body is empty" and "nobody
        // said how long the body is" call for different behaviour on a stream
        // that cannot be re-read.
        assert_eq!(declared_body_len(b"CSeq: 3\r\nContent-Length: 0\r\n\r\n"), Some(0));
        assert_eq!(declared_body_len(b"CSeq: 3\r\n\r\n"), None);
    }

    #[test]
    fn header_lookup_is_case_insensitive() {
        let h = "CSeq: 9\r\ncontent-length: 42\r\n";
        assert_eq!(header_value(h, "Content-Length"), Some("42"));
        assert_eq!(header_value(h, "CSeq"), Some("9"));
        assert_eq!(header_value(h, "Missing"), None);
    }

    #[test]
    fn a_buffer_holding_two_messages_yields_the_first_only() {
        let mut buf = sample(5);
        buf.extend_from_slice(&sample(6));
        let first = message_len(&buf).unwrap();
        assert_eq!(first, sample(5).len());
        assert_eq!(message_len(&buf[first..]), Some(sample(6).len()));
    }
}
