/// ChaCha20-Poly1305 AEAD framing for the encrypted RTSP control channel.
///
/// Wire format per packet:
///   uint16_le length N  ||  ciphertext (N bytes)  ||  Poly1305 tag (16 bytes)
///
/// The 2-byte little-endian length prefix is fed to the AEAD as associated
/// data (AAD) — verified against Shairport Sync (AirTunes/366.0) on hardware;
/// omitting it causes Poly1305 tag mismatch on both sides.
///
/// Nonce (12 bytes): [0x00 0x00 0x00 0x00] || counter (8 bytes, little-endian)
/// Each direction uses its own key and an independent monotonic counter.
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Key, Nonce,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ChaChaError {
    #[error("encryption failed")]
    Encrypt,
    #[error("decryption failed (tag mismatch or corrupt data)")]
    Decrypt,
    #[error("frame too short")]
    FrameTooShort,
}

/// One directional ChaCha20-Poly1305 channel (write or read).
pub struct ChaChaChannel {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl ChaChaChannel {
    pub fn new(key: &[u8; 32]) -> Self {
        ChaChaChannel {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key)),
            counter: 0,
        }
    }

    /// Encrypt `plaintext` and return the framed wire bytes:
    /// `uint16_le(len) || ciphertext || tag`.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, ChaChaError> {
        let len_bytes = (plaintext.len() as u16).to_le_bytes();
        let nonce = self.nonce();
        let ct = self
            .cipher
            .encrypt(&nonce, Payload { msg: plaintext, aad: &len_bytes })
            .map_err(|_| ChaChaError::Encrypt)?;
        self.counter += 1;

        // ct already includes the 16-byte tag appended by the AEAD crate.
        let mut out = Vec::with_capacity(2 + ct.len());
        out.extend_from_slice(&len_bytes);
        out.extend_from_slice(&ct);
        Ok(out)
    }

    /// Decrypt a framed message. `frame` should be exactly the bytes returned by `encrypt`.
    pub fn decrypt(&mut self, frame: &[u8]) -> Result<Vec<u8>, ChaChaError> {
        if frame.len() < 2 {
            return Err(ChaChaError::FrameTooShort);
        }
        let payload_len = u16::from_le_bytes([frame[0], frame[1]]) as usize;
        let expected = 2 + payload_len + 16;
        if frame.len() < expected {
            return Err(ChaChaError::FrameTooShort);
        }
        let ct_and_tag = &frame[2..expected];
        let nonce = self.nonce();
        let pt = self
            .cipher
            .decrypt(&nonce, Payload { msg: ct_and_tag, aad: &frame[..2] })
            .map_err(|_| ChaChaError::Decrypt)?;
        self.counter += 1;
        Ok(pt)
    }

    fn nonce(&self) -> Nonce {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&self.counter.to_le_bytes());
        *Nonce::from_slice(&n)
    }
}

/// One-shot AEAD seal with an ASCII label nonce (HomeKit pairing messages).
///
/// Nonce (12 bytes) = 4 zero bytes || 8-byte label (e.g. b"PS-Msg05").
/// No AAD. Output = ciphertext || 16-byte Poly1305 tag.
/// Verified against pyatv's Chacha20Cipher8byteNonce (leading-zero pad, aad=None).
pub fn seal_labeled(
    key: &[u8; 32],
    label8: &[u8; 8],
    plaintext: &[u8],
) -> Result<Vec<u8>, ChaChaError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .encrypt(&label_nonce(label8), plaintext)
        .map_err(|_| ChaChaError::Encrypt)
}

/// One-shot AEAD open with an ASCII label nonce (HomeKit pairing messages).
/// `ciphertext` must include the trailing 16-byte tag.
pub fn open_labeled(
    key: &[u8; 32],
    label8: &[u8; 8],
    ciphertext: &[u8],
) -> Result<Vec<u8>, ChaChaError> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(&label_nonce(label8), ciphertext)
        .map_err(|_| ChaChaError::Decrypt)
}

fn label_nonce(label8: &[u8; 8]) -> Nonce {
    let mut n = [0u8; 12];
    n[4..].copy_from_slice(label8);
    *Nonce::from_slice(&n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_key() -> [u8; 32] {
        [0x42u8; 32]
    }

    #[test]
    fn roundtrip() {
        let mut enc = ChaChaChannel::new(&test_key());
        let mut dec = ChaChaChannel::new(&test_key());
        let msg = b"GET /info RTSP/1.0\r\n\r\n";
        let frame = enc.encrypt(msg).unwrap();
        let plain = dec.decrypt(&frame).unwrap();
        assert_eq!(plain, msg);
    }

    #[test]
    fn counter_advances() {
        let mut enc = ChaChaChannel::new(&test_key());
        let mut dec = ChaChaChannel::new(&test_key());
        for i in 0u8..5 {
            let msg = vec![i; 16];
            let frame = enc.encrypt(&msg).unwrap();
            let plain = dec.decrypt(&frame).unwrap();
            assert_eq!(plain, msg);
        }
    }

    #[test]
    fn wrong_key_fails() {
        let mut enc = ChaChaChannel::new(&[0x42u8; 32]);
        let mut dec = ChaChaChannel::new(&[0x99u8; 32]);
        let frame = enc.encrypt(b"hello").unwrap();
        assert!(dec.decrypt(&frame).is_err());
    }

    #[test]
    fn labeled_seal_open_roundtrip() {
        let key = test_key();
        let ct = seal_labeled(&key, b"PS-Msg05", b"hello homekit").unwrap();
        assert_eq!(ct.len(), 13 + 16); // plaintext + tag
        let pt = open_labeled(&key, b"PS-Msg05", &ct).unwrap();
        assert_eq!(pt, b"hello homekit");
        // Wrong label → tag mismatch
        assert!(open_labeled(&key, b"PS-Msg06", &ct).is_err());
    }

    #[test]
    fn replayed_nonce_fails() {
        // If dec counter is behind, decryption should fail (nonce mismatch → tag fail).
        let mut enc = ChaChaChannel::new(&test_key());
        let mut dec = ChaChaChannel::new(&test_key());
        let f1 = enc.encrypt(b"first").unwrap();
        let f2 = enc.encrypt(b"second").unwrap();
        dec.decrypt(&f1).unwrap();
        dec.decrypt(&f2).unwrap();
        // Re-decrypting f1 with counter now at 2 → tag mismatch
        assert!(dec.decrypt(&f1).is_err());
    }
}
