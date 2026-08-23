/// HomeKit Transient pairing (X-Apple-HKP: 4).
///
/// Flow (wire format verified against Shairport Sync on hardware):
///   M1: client sends Method=0x00 + State=0x01 + Flags(0x13)=0x10 (Transient)
///   M2: server sends B (public key) + salt
///   M3: client sends A + M1 proof + state=0x03
///   M4: server sends M2 proof + state=0x04
///   Keys derived from session key via HKDF-SHA-512 → encrypted RTSP begins
///
/// No M5/M6, no pair-verify — Transient pairing stops at M4.
use openair_crypto::{hkdf_derive, SrpClient};
use thiserror::Error;

use crate::tlv8::{self, Tag};

#[derive(Debug, Error)]
pub enum PairingError {
    #[error("server returned TLV8 error code {0:#04x}")]
    ServerError(u8),
    #[error("missing TLV8 field: {0}")]
    MissingField(&'static str),
    #[error("SRP error: {0}")]
    Srp(#[from] openair_crypto::SrpError),
    #[error("server M2 proof verification failed")]
    M2Mismatch,
    #[error("unexpected state byte {got:#04x}, want {want:#04x}")]
    UnexpectedState { got: u8, want: u8 },
    #[error("crypto failure: {0}")]
    CryptoFailure(&'static str),
    #[error("signature verification failed: {0}")]
    SignatureInvalid(&'static str),
    #[error("accessory identity does not match stored credentials")]
    PeerMismatch,
}

/// HomeKit TLV8 error codes (HAP specification, table 5-5).
///
/// Only the ones we act on differently are named; the rest reach the user as
/// the raw code, which is still better than a generic failure.
pub mod error_code {
    /// The accessory refused our credentials.
    ///
    /// In pair-setup this is a wrong PIN. In pair-verify it means the
    /// accessory has no record of us -- it was reset, or the pairing was
    /// removed on its side -- which is a different problem with a different
    /// remedy, so the two are told apart by the caller, not here.
    pub const AUTHENTICATION: u8 = 0x02;
    /// Too many attempts; the accessory wants us to wait.
    pub const MAX_TRIES: u8 = 0x05;
}

impl PairingError {
    /// Whether this is the accessory rejecting who we are, as opposed to a
    /// network, protocol or crypto failure.
    ///
    /// `PeerMismatch` counts: the accessory answered with an identity that is
    /// not the one we stored, which is what a factory-reset receiver looks
    /// like from here.
    pub fn is_authentication(&self) -> bool {
        matches!(
            self,
            PairingError::ServerError(error_code::AUTHENTICATION) | PairingError::PeerMismatch
        )
    }
}

/// Result of the M2→M3 SRP step: `(m3_body, m1_proof, srp_session_key)`.
pub type SrpStepResult = Result<(Vec<u8>, Vec<u8>, Vec<u8>), PairingError>;

/// Channel keys derived at the end of pairing.
/// Write = client → server, Read = server → client.
#[derive(Debug)]
pub struct PairingKeys {
    pub write: [u8; 32],
    pub read: [u8; 32],
    /// Keys for the reverse *event* channel, derived from the same shared
    /// secret but with the `Events-Salt` label set. Apple receivers push
    /// encrypted messages to us on that channel; without these we cannot read
    /// them, and an Apple TV tears the session down after ~30 s of silence.
    pub events_write: [u8; 32],
    pub events_read: [u8; 32],
}

/// Derive the four channel keys from a shared secret.
///
/// Control and Events use the identical HKDF construction, differing only in
/// the salt/info labels — so they are derived together to keep the two label
/// sets visibly parallel.
pub(crate) fn derive_channel_keys(shared: &[u8]) -> PairingKeys {
    PairingKeys {
        write: hkdf_derive(shared, b"Control-Salt", b"Control-Write-Encryption-Key"),
        read: hkdf_derive(shared, b"Control-Salt", b"Control-Read-Encryption-Key"),
        events_write: hkdf_derive(shared, b"Events-Salt", b"Events-Write-Encryption-Key"),
        events_read: hkdf_derive(shared, b"Events-Salt", b"Events-Read-Encryption-Key"),
    }
}

/// The pairing handshake, broken into two steps matching the two HTTP round-trips.
pub struct TransientPairing {
    client: SrpClient,
}

impl TransientPairing {
    pub fn new() -> Self {
        TransientPairing {
            client: SrpClient::new(b"Pair-Setup", b"3939"),
        }
    }

    /// Build the M1 TLV8 body (POST /pair-setup, first round-trip).
    ///
    /// Exactly Method=0x00, State=0x01, Flags(0x13)=0x10 — nothing else.
    /// The Transient flag (0x10) is what makes the receiver use PIN "3939";
    /// without it, pair_ap treats the session as normal pairing and the SRP
    /// proof fails with kTLVError_Authentication (0x02).
    /// A is NOT sent in M1 — it goes in M3 alongside the proof.
    pub fn build_m1(&self) -> Vec<u8> {
        tlv8::encode(&[
            (Tag::Method, &[0x00]),                  // Pair Setup, no MFi
            (Tag::State,  &[0x01]),                  // M1
            (Tag::Flags,  &[tlv8::FLAG_TRANSIENT]),  // 0x10 = Transient
        ])
    }

    /// Parse the M2 response, compute M3, return the TLV8 body for the second POST.
    /// Also returns M1 proof and session key so we can verify M4 and derive keys.
    pub fn process_m2_build_m3(&self, m2_body: &[u8]) -> SrpStepResult {
        let tlv = tlv8::decode(m2_body);

        // Check for server error first.
        if let Some(err) = tlv.get(&(Tag::Error as u8)) {
            return Err(PairingError::ServerError(*err.first().unwrap_or(&0xFF)));
        }

        let state = tlv
            .get(&(Tag::State as u8))
            .and_then(|v| v.first().copied())
            .ok_or(PairingError::MissingField("State"))?;
        if state != 0x02 {
            return Err(PairingError::UnexpectedState { got: state, want: 0x02 });
        }

        let b_pub = tlv
            .get(&(Tag::PublicKey as u8))
            .ok_or(PairingError::MissingField("PublicKey(B)"))?;
        let salt = tlv
            .get(&(Tag::Salt as u8))
            .ok_or(PairingError::MissingField("Salt"))?;

        let (m1_proof, session_key) = self.client.process_challenge(
            b"Pair-Setup", b"3939", salt, b_pub,
        )?;

        // A is sent here in M3 (not M1), padded to 384 bytes.
        let a_bytes = self.client.a_pub_padded();
        let m3_body = tlv8::encode(&[
            (Tag::State,     &[0x03]),
            (Tag::PublicKey, &a_bytes),
            (Tag::Proof,     &m1_proof),
        ]);

        Ok((m3_body, m1_proof, session_key))
    }

    /// Parse the M4 response, verify the server's proof, derive channel keys.
    pub fn process_m4(
        &self,
        m4_body: &[u8],
        m1_proof: &[u8],
        session_key: &[u8],
    ) -> Result<PairingKeys, PairingError> {
        let tlv = tlv8::decode(m4_body);

        if let Some(err) = tlv.get(&(Tag::Error as u8)) {
            return Err(PairingError::ServerError(*err.first().unwrap_or(&0xFF)));
        }

        let state = tlv
            .get(&(Tag::State as u8))
            .and_then(|v| v.first().copied())
            .ok_or(PairingError::MissingField("State"))?;
        if state != 0x04 {
            return Err(PairingError::UnexpectedState { got: state, want: 0x04 });
        }

        let m2_proof = tlv
            .get(&(Tag::Proof as u8))
            .ok_or(PairingError::MissingField("Proof(M2)"))?;

        // Verify the server's M2 proof = H(A_unpadded || M1 || K).
        // Hardware-verified: Shairport Sync sends a real 64-byte proof in M4
        // once M1 carries the correct Transient flag.
        self.client.verify_server(m1_proof, m2_proof, session_key)?;

        // Derive per-direction RTSP + event channel keys from session key K.
        Ok(derive_channel_keys(session_key))
    }
}

impl Default for TransientPairing {
    fn default() -> Self { Self::new() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv8;

    #[test]
    fn only_a_rejected_identity_counts_as_authentication() {
        assert!(PairingError::ServerError(error_code::AUTHENTICATION).is_authentication());
        // A reset accessory answers with an identity that is not the one we
        // stored, which is the same problem wearing a different error.
        assert!(PairingError::PeerMismatch.is_authentication());
    }

    #[test]
    fn transport_and_protocol_failures_are_not_authentication() {
        // These must stay distinguishable: treating a busy accessory or a
        // truncated message as "it forgot us" would send the user off to
        // re-pair a receiver whose pairing was never the problem.
        for e in [
            PairingError::ServerError(error_code::MAX_TRIES),
            PairingError::ServerError(0x07), // Busy
            PairingError::MissingField("State"),
            PairingError::M2Mismatch,
            PairingError::UnexpectedState { got: 3, want: 2 },
            PairingError::CryptoFailure("bad length"),
            PairingError::SignatureInvalid("accessory"),
        ] {
            assert!(!e.is_authentication(), "{e:?} should not be authentication");
        }
    }

    #[test]
    fn m1_matches_hardware_verified_wire_format() {
        let p = TransientPairing::new();
        let m1 = p.build_m1();
        // Exact bytes verified against Shairport Sync:
        // Method(0x00)=0x00, State(0x06)=0x01, Flags(0x13)=0x10
        assert_eq!(m1, vec![0x00, 1, 0x00, 0x06, 1, 0x01, 0x13, 1, 0x10]);
    }

    #[test]
    fn m2_missing_field_errors() {
        let p = TransientPairing::new();
        // M2 with state=0x02 but no B or salt
        let m2 = tlv8::encode(&[(Tag::State, &[0x02])]);
        assert!(matches!(
            p.process_m2_build_m3(&m2),
            Err(PairingError::MissingField(_))
        ));
    }

    #[test]
    fn m2_wrong_state_errors() {
        let p = TransientPairing::new();
        let m2 = tlv8::encode(&[
            (Tag::State, &[0x05]),
            (Tag::PublicKey, &[0u8; 32]),
            (Tag::Salt, &[0u8; 16]),
        ]);
        assert!(matches!(
            p.process_m2_build_m3(&m2),
            Err(PairingError::UnexpectedState { .. })
        ));
    }

    #[test]
    fn server_error_in_m2_propagates() {
        let p = TransientPairing::new();
        let m2 = tlv8::encode(&[
            (Tag::State, &[0x02]),
            (Tag::Error, &[0x02]), // kTLVError_Authentication
        ]);
        assert!(matches!(
            p.process_m2_build_m3(&m2),
            Err(PairingError::ServerError(0x02))
        ));
    }
}
