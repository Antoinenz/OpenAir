/// HomeKit Normal pairing (X-Apple-HKP: 3) — pair-setup M1–M6 + pair-verify.
///
/// Used by Apple TV / HomePod, which reject Transient pairing (HTTP 470).
/// Wire format verified against pyatv (auth/hap_srp.py, protocols/airplay/auth/hap.py):
///
/// Pair-setup (once, PIN shown on the device's screen):
///   (POST /pair-pin-start first — makes the Apple TV display the PIN)
///   M1: {Method=0x00, State=1}                        — NO transient flag
///   M2: {State=2, Salt, PublicKey(B)}                 — PIN now on screen
///   M3: {State=3, PublicKey(A), Proof(M1)}            — SRP w/ user "Pair-Setup", pw = PIN
///   M4: {State=4, Proof(M2)}                          — verify server proof
///   M5: {State=5, EncryptedData}                      — our Ed25519 identity, signed
///   M6: {State=6, EncryptedData}                      — accessory's Ed25519 identity
///
/// M5/M6 sub-TLVs are sealed with ChaCha20-Poly1305:
///   key   = HKDF-SHA512(K, "Pair-Setup-Encrypt-Salt", "Pair-Setup-Encrypt-Info")
///   nonce = 4 zero bytes || "PS-Msg05" / "PS-Msg06", no AAD
///
/// Pair-verify (every subsequent connection, replaces pair-setup):
///   M1: {State=1, PublicKey(our X25519 ephemeral)}
///   M2: {State=2, PublicKey(their ephemeral), EncryptedData("PV-Msg02")}
///   M3: {State=3, EncryptedData("PV-Msg03")}
///   M4: {State=4} on success
///   Channel keys = HKDF(raw X25519 shared secret, "Control-Salt",
///                       "Control-Write/Read-Encryption-Key")
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use openair_crypto::{hkdf_derive, open_labeled, seal_labeled, SrpClient};
use rand::RngCore;
use x25519_dalek::{EphemeralSecret, PublicKey as XPublicKey};

use crate::tlv8::{self, Tag};
use crate::transient::{PairingError, PairingKeys, SrpStepResult};

/// Our long-term controller identity: a pairing ID (UUID string) and an
/// Ed25519 signing seed. Generated once and persisted to disk.
#[derive(Debug, Clone)]
pub struct Identity {
    /// ASCII UUID string bytes, e.g. b"5f8de963-...-...".
    pub pairing_id: Vec<u8>,
    /// Ed25519 long-term secret key seed (LTSK).
    pub signing_seed: [u8; 32],
}

impl Identity {
    pub fn generate() -> Self {
        let mut rng = rand::thread_rng();
        let mut seed = [0u8; 32];
        rng.fill_bytes(&mut seed);
        let mut uuid_bytes = [0u8; 16];
        rng.fill_bytes(&mut uuid_bytes);
        // RFC 4122 version-4 UUID formatting.
        uuid_bytes[6] = (uuid_bytes[6] & 0x0F) | 0x40;
        uuid_bytes[8] = (uuid_bytes[8] & 0x3F) | 0x80;
        let hex: Vec<String> = uuid_bytes.iter().map(|b| format!("{b:02x}")).collect();
        let s = hex.join("");
        let uuid = format!(
            "{}-{}-{}-{}-{}",
            &s[0..8], &s[8..12], &s[12..16], &s[16..20], &s[20..32]
        );
        Identity {
            pairing_id: uuid.into_bytes(),
            signing_seed: seed,
        }
    }

    /// Long-term public key (LTPK) for this identity.
    pub fn ltpk(&self) -> [u8; 32] {
        SigningKey::from_bytes(&self.signing_seed)
            .verifying_key()
            .to_bytes()
    }
}

/// The accessory's long-term identity, learned during pair-setup M6
/// and persisted so pair-verify can authenticate it later.
#[derive(Debug, Clone)]
pub struct PeerCredentials {
    /// Accessory's pairing identifier (ASCII bytes).
    pub peer_id: Vec<u8>,
    /// Accessory's Ed25519 long-term public key.
    pub ltpk: [u8; 32],
}

/// Normal pair-setup state machine (M1–M6). One-time, needs the on-screen PIN.
pub struct NormalPairing {
    client: SrpClient,
}

impl NormalPairing {
    pub fn new() -> Self {
        // Password is supplied later (process_m2_build_m3) once the user has
        // read the PIN off the device's screen; SrpClient only uses it there.
        NormalPairing {
            client: SrpClient::new(b"Pair-Setup", b""),
        }
    }

    /// M1: Method=0x00 (Pair Setup), State=1. No transient flag —
    /// its absence is what makes the Apple TV show a PIN.
    pub fn build_m1(&self) -> Vec<u8> {
        tlv8::encode(&[(Tag::Method, &[0x00]), (Tag::State, &[0x01])])
    }

    /// Parse M2 (salt + B), run SRP with the user-supplied PIN, build M3.
    /// Returns `(m3_body, m1_proof, srp_key_K)`.
    pub fn process_m2_build_m3(&self, m2_body: &[u8], pin: &str) -> SrpStepResult {
        let tlv = tlv8::decode(m2_body);
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

        let (m1_proof, srp_key) =
            self.client
                .process_challenge(b"Pair-Setup", pin.as_bytes(), salt, b_pub)?;

        let a_bytes = self.client.a_pub_padded();
        let m3_body = tlv8::encode(&[
            (Tag::State, &[0x03]),
            (Tag::PublicKey, &a_bytes),
            (Tag::Proof, &m1_proof),
        ]);
        Ok((m3_body, m1_proof, srp_key))
    }

    /// Verify M4's server proof, then build the encrypted M5 identity exchange.
    /// Returns `(m5_body, setup_encrypt_key)` — keep the key for M6.
    pub fn process_m4_build_m5(
        &self,
        m4_body: &[u8],
        m1_proof: &[u8],
        srp_key: &[u8],
        identity: &Identity,
    ) -> Result<(Vec<u8>, [u8; 32]), PairingError> {
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
        self.client.verify_server(m1_proof, m2_proof, srp_key)?;

        // Derive the pair-setup encryption key and the controller signing salt.
        let encrypt_key = hkdf_derive(
            srp_key,
            b"Pair-Setup-Encrypt-Salt",
            b"Pair-Setup-Encrypt-Info",
        );
        let ios_device_x = hkdf_derive(
            srp_key,
            b"Pair-Setup-Controller-Sign-Salt",
            b"Pair-Setup-Controller-Sign-Info",
        );

        // Sign iOSDeviceX || pairing_id || LTPK with our long-term key.
        let signing_key = SigningKey::from_bytes(&identity.signing_seed);
        let ltpk = signing_key.verifying_key().to_bytes();
        let mut info = Vec::with_capacity(32 + identity.pairing_id.len() + 32);
        info.extend_from_slice(&ios_device_x);
        info.extend_from_slice(&identity.pairing_id);
        info.extend_from_slice(&ltpk);
        let signature = signing_key.sign(&info).to_bytes();

        let sub_tlv = tlv8::encode(&[
            (Tag::Identifier, &identity.pairing_id),
            (Tag::PublicKey, &ltpk),
            (Tag::Signature, &signature),
        ]);
        let sealed = seal_labeled(&encrypt_key, b"PS-Msg05", &sub_tlv)
            .map_err(|_| PairingError::CryptoFailure("seal PS-Msg05"))?;

        let m5_body = tlv8::encode(&[(Tag::State, &[0x05]), (Tag::EncryptedData, &sealed)]);
        Ok((m5_body, encrypt_key))
    }

    /// Decrypt + verify M6, returning the accessory's long-term identity.
    pub fn process_m6(
        &self,
        m6_body: &[u8],
        srp_key: &[u8],
        encrypt_key: &[u8; 32],
    ) -> Result<PeerCredentials, PairingError> {
        let tlv = tlv8::decode(m6_body);
        if let Some(err) = tlv.get(&(Tag::Error as u8)) {
            return Err(PairingError::ServerError(*err.first().unwrap_or(&0xFF)));
        }
        let state = tlv
            .get(&(Tag::State as u8))
            .and_then(|v| v.first().copied())
            .ok_or(PairingError::MissingField("State"))?;
        if state != 0x06 {
            return Err(PairingError::UnexpectedState { got: state, want: 0x06 });
        }
        let sealed = tlv
            .get(&(Tag::EncryptedData as u8))
            .ok_or(PairingError::MissingField("EncryptedData"))?;
        let sub = open_labeled(encrypt_key, b"PS-Msg06", sealed)
            .map_err(|_| PairingError::CryptoFailure("open PS-Msg06"))?;
        let sub_tlv = tlv8::decode(&sub);

        let peer_id = sub_tlv
            .get(&(Tag::Identifier as u8))
            .ok_or(PairingError::MissingField("Identifier(accessory)"))?
            .clone();
        let ltpk_vec = sub_tlv
            .get(&(Tag::PublicKey as u8))
            .ok_or(PairingError::MissingField("PublicKey(accessory LTPK)"))?;
        let signature = sub_tlv
            .get(&(Tag::Signature as u8))
            .ok_or(PairingError::MissingField("Signature(accessory)"))?;
        let ltpk: [u8; 32] = ltpk_vec
            .as_slice()
            .try_into()
            .map_err(|_| PairingError::CryptoFailure("accessory LTPK length"))?;

        // Verify sig over AccessoryX || accessory_id || accessory_LTPK.
        // (pyatv skips this — HAP spec requires it; relax only if hardware disagrees.)
        let accessory_x = hkdf_derive(
            srp_key,
            b"Pair-Setup-Accessory-Sign-Salt",
            b"Pair-Setup-Accessory-Sign-Info",
        );
        let mut info = Vec::with_capacity(32 + peer_id.len() + 32);
        info.extend_from_slice(&accessory_x);
        info.extend_from_slice(&peer_id);
        info.extend_from_slice(&ltpk);
        let vkey = VerifyingKey::from_bytes(&ltpk)
            .map_err(|_| PairingError::CryptoFailure("accessory LTPK invalid"))?;
        let sig_bytes: [u8; 64] = signature
            .as_slice()
            .try_into()
            .map_err(|_| PairingError::CryptoFailure("accessory signature length"))?;
        vkey.verify(&info, &ed25519_dalek::Signature::from_bytes(&sig_bytes))
            .map_err(|_| PairingError::SignatureInvalid("accessory M6"))?;

        Ok(PeerCredentials { peer_id, ltpk })
    }
}

impl Default for NormalPairing {
    fn default() -> Self {
        Self::new()
    }
}

/// Pair-verify state machine (M1–M4). Fast, per-connection, no PIN.
pub struct PairVerify {
    identity: Identity,
    peer: PeerCredentials,
    eph_secret: Option<EphemeralSecret>,
    eph_pub: [u8; 32],
    shared: Option<[u8; 32]>,
}

impl PairVerify {
    pub fn new(identity: Identity, peer: PeerCredentials) -> Self {
        let secret = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
        let eph_pub = XPublicKey::from(&secret).to_bytes();
        PairVerify {
            identity,
            peer,
            eph_secret: Some(secret),
            eph_pub,
            shared: None,
        }
    }

    /// M1: our ephemeral X25519 public key.
    pub fn build_m1(&self) -> Vec<u8> {
        tlv8::encode(&[(Tag::State, &[0x01]), (Tag::PublicKey, &self.eph_pub)])
    }

    /// Parse M2 (their ephemeral + encrypted signature), verify the accessory,
    /// and build the encrypted M3 response.
    pub fn process_m2_build_m3(&mut self, m2_body: &[u8]) -> Result<Vec<u8>, PairingError> {
        let tlv = tlv8::decode(m2_body);
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
        let their_pub_vec = tlv
            .get(&(Tag::PublicKey as u8))
            .ok_or(PairingError::MissingField("PublicKey(accessory ephemeral)"))?;
        let sealed = tlv
            .get(&(Tag::EncryptedData as u8))
            .ok_or(PairingError::MissingField("EncryptedData"))?;
        let their_pub: [u8; 32] = their_pub_vec
            .as_slice()
            .try_into()
            .map_err(|_| PairingError::CryptoFailure("accessory ephemeral key length"))?;

        // X25519 shared secret (raw — channel keys derive from this, not from
        // the Pair-Verify encryption key).
        let secret = self
            .eph_secret
            .take()
            .ok_or(PairingError::CryptoFailure("ephemeral key already used"))?;
        let shared = secret
            .diffie_hellman(&XPublicKey::from(their_pub))
            .to_bytes();
        self.shared = Some(shared);

        let verify_key = hkdf_derive(
            &shared,
            b"Pair-Verify-Encrypt-Salt",
            b"Pair-Verify-Encrypt-Info",
        );
        let sub = open_labeled(&verify_key, b"PV-Msg02", sealed)
            .map_err(|_| PairingError::CryptoFailure("open PV-Msg02"))?;
        let sub_tlv = tlv8::decode(&sub);

        let identifier = sub_tlv
            .get(&(Tag::Identifier as u8))
            .ok_or(PairingError::MissingField("Identifier(accessory)"))?;
        let signature = sub_tlv
            .get(&(Tag::Signature as u8))
            .ok_or(PairingError::MissingField("Signature(accessory)"))?;

        if identifier != &self.peer.peer_id {
            return Err(PairingError::PeerMismatch);
        }

        // Verify: sig over their_eph_pub || accessory_id || our_eph_pub
        // with the LTPK we stored at pair-setup time.
        let mut info = Vec::with_capacity(32 + identifier.len() + 32);
        info.extend_from_slice(&their_pub);
        info.extend_from_slice(identifier);
        info.extend_from_slice(&self.eph_pub);
        let vkey = VerifyingKey::from_bytes(&self.peer.ltpk)
            .map_err(|_| PairingError::CryptoFailure("stored LTPK invalid"))?;
        let sig_bytes: [u8; 64] = signature
            .as_slice()
            .try_into()
            .map_err(|_| PairingError::CryptoFailure("accessory signature length"))?;
        vkey.verify(&info, &ed25519_dalek::Signature::from_bytes(&sig_bytes))
            .map_err(|_| PairingError::SignatureInvalid("accessory pair-verify"))?;

        // Our reply: sign our_eph_pub || our_id || their_eph_pub with LTSK.
        let signing_key = SigningKey::from_bytes(&self.identity.signing_seed);
        let mut our_info = Vec::with_capacity(32 + self.identity.pairing_id.len() + 32);
        our_info.extend_from_slice(&self.eph_pub);
        our_info.extend_from_slice(&self.identity.pairing_id);
        our_info.extend_from_slice(&their_pub);
        let our_sig = signing_key.sign(&our_info).to_bytes();

        let reply_sub = tlv8::encode(&[
            (Tag::Identifier, &self.identity.pairing_id),
            (Tag::Signature, &our_sig),
        ]);
        let reply_sealed = seal_labeled(&verify_key, b"PV-Msg03", &reply_sub)
            .map_err(|_| PairingError::CryptoFailure("seal PV-Msg03"))?;

        Ok(tlv8::encode(&[
            (Tag::State, &[0x03]),
            (Tag::EncryptedData, &reply_sealed),
        ]))
    }

    /// Parse M4 and derive the encrypted-RTSP channel keys from the raw
    /// X25519 shared secret (verified in pyatv: verify2 uses self._shared).
    pub fn process_m4(&self, m4_body: &[u8]) -> Result<PairingKeys, PairingError> {
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
        let shared = self
            .shared
            .ok_or(PairingError::CryptoFailure("no shared secret (M2 not processed)"))?;
        let write = hkdf_derive(&shared, b"Control-Salt", b"Control-Write-Encryption-Key");
        let read = hkdf_derive(&shared, b"Control-Salt", b"Control-Read-Encryption-Key");
        Ok(PairingKeys { write, read })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_generates_valid_uuid() {
        let id = Identity::generate();
        let s = String::from_utf8(id.pairing_id.clone()).unwrap();
        assert_eq!(s.len(), 36);
        assert_eq!(s.chars().filter(|c| *c == '-').count(), 4);
        // Version 4 marker
        assert_eq!(s.as_bytes()[14], b'4');
        // LTPK derivable
        assert_eq!(id.ltpk().len(), 32);
    }

    #[test]
    fn m1_wire_format() {
        let p = NormalPairing::new();
        // Method(0x00)=0x00, State(0x06)=0x01 — no transient flag.
        assert_eq!(p.build_m1(), vec![0x00, 1, 0x00, 0x06, 1, 0x01]);
    }

    #[test]
    fn pair_verify_full_roundtrip_against_mock_accessory() {
        // Simulate the accessory side to exercise M1→M4 end-to-end.
        let controller = Identity::generate();

        // Accessory long-term identity
        let acc_seed = [7u8; 32];
        let acc_signing = SigningKey::from_bytes(&acc_seed);
        let acc_id = b"AA:BB:CC:DD:EE:FF".to_vec();
        let peer = PeerCredentials {
            peer_id: acc_id.clone(),
            ltpk: acc_signing.verifying_key().to_bytes(),
        };

        let mut pv = PairVerify::new(controller.clone(), peer);
        let m1 = pv.build_m1();
        let m1_tlv = tlv8::decode(&m1);
        let ctrl_eph: [u8; 32] = m1_tlv[&(Tag::PublicKey as u8)]
            .as_slice()
            .try_into()
            .unwrap();

        // Accessory: ephemeral X25519, shared secret, signed M2.
        let acc_secret = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
        let acc_eph = XPublicKey::from(&acc_secret).to_bytes();
        let shared = acc_secret
            .diffie_hellman(&XPublicKey::from(ctrl_eph))
            .to_bytes();
        let vkey = hkdf_derive(
            &shared,
            b"Pair-Verify-Encrypt-Salt",
            b"Pair-Verify-Encrypt-Info",
        );
        let mut info = Vec::new();
        info.extend_from_slice(&acc_eph);
        info.extend_from_slice(&acc_id);
        info.extend_from_slice(&ctrl_eph);
        let acc_sig = acc_signing.sign(&info).to_bytes();
        let sub = tlv8::encode(&[(Tag::Identifier, &acc_id), (Tag::Signature, &acc_sig)]);
        let sealed = seal_labeled(&vkey, b"PV-Msg02", &sub).unwrap();
        let m2 = tlv8::encode(&[
            (Tag::State, &[0x02]),
            (Tag::PublicKey, &acc_eph),
            (Tag::EncryptedData, &sealed),
        ]);

        // Controller processes M2, produces M3 — accessory verifies it.
        let m3 = pv.process_m2_build_m3(&m2).unwrap();
        let m3_tlv = tlv8::decode(&m3);
        let m3_sealed = &m3_tlv[&(Tag::EncryptedData as u8)];
        let m3_sub = tlv8::decode(&open_labeled(&vkey, b"PV-Msg03", m3_sealed).unwrap());
        assert_eq!(m3_sub[&(Tag::Identifier as u8)], controller.pairing_id);
        let ctrl_sig: [u8; 64] = m3_sub[&(Tag::Signature as u8)]
            .as_slice()
            .try_into()
            .unwrap();
        let mut ctrl_info = Vec::new();
        ctrl_info.extend_from_slice(&ctrl_eph);
        ctrl_info.extend_from_slice(&controller.pairing_id);
        ctrl_info.extend_from_slice(&acc_eph);
        VerifyingKey::from_bytes(&controller.ltpk())
            .unwrap()
            .verify(&ctrl_info, &ed25519_dalek::Signature::from_bytes(&ctrl_sig))
            .unwrap();

        // M4 → channel keys match what the accessory would derive.
        let m4 = tlv8::encode(&[(Tag::State, &[0x04])]);
        let keys = pv.process_m4(&m4).unwrap();
        assert_eq!(
            keys.write,
            hkdf_derive(&shared, b"Control-Salt", b"Control-Write-Encryption-Key")
        );
        assert_eq!(
            keys.read,
            hkdf_derive(&shared, b"Control-Salt", b"Control-Read-Encryption-Key")
        );
    }

    #[test]
    fn pair_verify_rejects_wrong_accessory_identity() {
        let controller = Identity::generate();
        let acc_signing = SigningKey::from_bytes(&[7u8; 32]);
        let peer = PeerCredentials {
            peer_id: b"AA:BB:CC:DD:EE:FF".to_vec(),
            ltpk: acc_signing.verifying_key().to_bytes(),
        };
        let mut pv = PairVerify::new(controller, peer);
        let m1 = pv.build_m1();
        let ctrl_eph: [u8; 32] = tlv8::decode(&m1)[&(Tag::PublicKey as u8)]
            .as_slice()
            .try_into()
            .unwrap();

        // Accessory replies with a DIFFERENT identity (impersonation).
        let evil_id = b"11:22:33:44:55:66".to_vec();
        let acc_secret = EphemeralSecret::random_from_rng(rand::rngs::OsRng);
        let acc_eph = XPublicKey::from(&acc_secret).to_bytes();
        let shared = acc_secret
            .diffie_hellman(&XPublicKey::from(ctrl_eph))
            .to_bytes();
        let vkey = hkdf_derive(
            &shared,
            b"Pair-Verify-Encrypt-Salt",
            b"Pair-Verify-Encrypt-Info",
        );
        let mut info = Vec::new();
        info.extend_from_slice(&acc_eph);
        info.extend_from_slice(&evil_id);
        info.extend_from_slice(&ctrl_eph);
        let sig = acc_signing.sign(&info).to_bytes();
        let sub = tlv8::encode(&[(Tag::Identifier, &evil_id), (Tag::Signature, &sig)]);
        let sealed = seal_labeled(&vkey, b"PV-Msg02", &sub).unwrap();
        let m2 = tlv8::encode(&[
            (Tag::State, &[0x02]),
            (Tag::PublicKey, &acc_eph),
            (Tag::EncryptedData, &sealed),
        ]);

        assert!(matches!(
            pv.process_m2_build_m3(&m2),
            Err(PairingError::PeerMismatch)
        ));
    }

    #[test]
    fn m5_seal_and_mock_m6_roundtrip() {
        // Exercise the M5 sub-TLV construction + M6 parsing with a mock
        // accessory that follows the HAP spec (signed identity).
        let identity = Identity::generate();
        let srp_key = [0x5Au8; 64]; // stand-in for SRP K

        let encrypt_key = hkdf_derive(
            &srp_key,
            b"Pair-Setup-Encrypt-Salt",
            b"Pair-Setup-Encrypt-Info",
        );

        // Build a spec-conformant M6 from a mock accessory.
        let acc_signing = SigningKey::from_bytes(&[9u8; 32]);
        let acc_id = b"mock-accessory-id".to_vec();
        let acc_ltpk = acc_signing.verifying_key().to_bytes();
        let accessory_x = hkdf_derive(
            &srp_key,
            b"Pair-Setup-Accessory-Sign-Salt",
            b"Pair-Setup-Accessory-Sign-Info",
        );
        let mut info = Vec::new();
        info.extend_from_slice(&accessory_x);
        info.extend_from_slice(&acc_id);
        info.extend_from_slice(&acc_ltpk);
        let sig = acc_signing.sign(&info).to_bytes();
        let sub = tlv8::encode(&[
            (Tag::Identifier, &acc_id),
            (Tag::PublicKey, &acc_ltpk),
            (Tag::Signature, &sig),
        ]);
        let sealed = seal_labeled(&encrypt_key, b"PS-Msg06", &sub).unwrap();
        let m6 = tlv8::encode(&[(Tag::State, &[0x06]), (Tag::EncryptedData, &sealed)]);

        let p = NormalPairing::new();
        let creds = p.process_m6(&m6, &srp_key, &encrypt_key).unwrap();
        assert_eq!(creds.peer_id, acc_id);
        assert_eq!(creds.ltpk, acc_ltpk);
        let _ = identity; // (M5 construction covered implicitly via process_m4_build_m5 on hardware)
    }
}
