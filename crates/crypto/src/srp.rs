/// SRP-6a, 3072-bit group (RFC 5054 Appendix A), SHA-512.
///
/// Formulas match pair_ap (ejurgensen/pair_ap, embedded in mikebrady/shairport-sync):
///   k   = H(PAD(N, N_len) || PAD(g, N_len))        [H_nn_pad — g padded]
///   u   = H(PAD(A, N_len) || PAD(B, N_len))        [H_nn_pad — A,B padded]
///   S   = (B - k*v)^(a + u*x) mod N
///   K   = SHA512(BN_bn2bin(S))                      [hash_num — unpadded S, 64 bytes]
///   M1  = SHA512(H(N)^H(g) || H(I) || s || A || B || K)  [calculate_M — unpadded s,A,B]
///   M2  = SHA512(BN_bn2bin(A) || M1 || K)           [calculate_H_AMK — unpadded A]
///
/// Reference: ejurgensen/pair_ap/pair_homekit.c, srp.c
use num_bigint::BigUint;
use num_traits::{One, Zero};
use rand::RngCore;
use sha2::{Digest, Sha512};

/// RFC 5054 Appendix A / RFC 3526 §7 — 3072-bit SRP group.
/// 768 hex chars = 384 bytes = 3072 bits. Generator g = 5.
const N_HEX: &str = concat!(
    "FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD1",
    "29024E088A67CC74020BBEA63B139B22514A08798E3404DD",
    "EF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245",
    "E485B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7ED",
    "EE386BFB5A899FA5AE9F24117C4B1FE649286651ECE45B3D",
    "C2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F",
    "83655D23DCA3AD961C62F356208552BB9ED529077096966D",
    "670C354E4ABC9804F1746C08CA18217C32905E462E36CE3B",
    "E39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9",
    "DE2BCBF6955817183995497CEA956AE515D2261898FA0510",
    "15728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64",
    "ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6B",
    "F12FFA06D98A0864D87602733EC86A64521F2B18177B200C",
    "BBE117577A615D6C770988C0BAD946E208E24FA074E5AB31",
    "43DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF",
);

#[derive(Debug, thiserror::Error)]
pub enum SrpError {
    #[error("u = 0 (abort)")]
    InvalidU,
    #[error("server M2 verification failed")]
    M2Mismatch,
}

/// Client side of SRP-6a M1–M4 (Transient pairing stops here — no M5/M6).
pub struct SrpClient {
    n: BigUint,
    g: BigUint,
    a: BigUint,
    pub a_pub: BigUint,
}

impl SrpClient {
    /// `username` = "Pair-Setup", `password` = "3939" for Transient pairing.
    pub fn new(_username: &[u8], _password: &[u8]) -> Self {
        let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("valid N");
        let g = BigUint::from(5u32);
        let a = random_private_key(&n);
        let a_pub = g.modpow(&a, &n);
        SrpClient { n, g, a, a_pub }
    }

    /// Returns A padded to exactly 384 bytes (N_len).
    pub fn a_pub_padded(&self) -> Vec<u8> {
        padded(&self.a_pub, &self.n)
    }

    /// Compute M1 and K from the server's B and salt.
    ///
    /// Returns `(M1_proof [64 bytes], K [64 bytes])`.
    pub fn process_challenge(
        &self,
        username: &[u8],
        password: &[u8],
        salt: &[u8],
        b_pub_bytes: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>), SrpError> {
        let b_pub = BigUint::from_bytes_be(b_pub_bytes);
        let n = &self.n;
        let g = &self.g;

        // u = H_nn_pad(A, B) — both padded to N_len
        let u = h_nn_pad(&self.a_pub, &b_pub, n);
        if u.is_zero() {
            return Err(SrpError::InvalidU);
        }

        // x = H(salt || H(I || ":" || P))
        let x = compute_x(salt, username, password);

        // v = g^x mod N
        let v = g.modpow(&x, n);

        // k = H_nn_pad(N, g)
        let k = h_nn_pad_ng(n, g);

        // S = (B - k*v)^(a + u*x) mod N
        let kv = (&k * &v) % n;
        let b_minus_kv = if b_pub >= kv {
            (&b_pub - &kv) % n
        } else {
            (n + &b_pub - &kv) % n
        };
        let exp = &self.a + &u * &x;
        let s = b_minus_kv.modpow(&exp, n);

        // K = hash_num(S) = SHA512(BN_bn2bin(S)) — unpadded S
        let k_bytes = sha512(&s.to_bytes_be());

        // M1 = SHA512(H(N)^H(g) || H(I) || s || A || B || K)
        // All via update_hash_n (unpadded BN_bn2bin) except H_xor and K
        let m1 = compute_m1(n, g, username, salt, &self.a_pub, &b_pub, &k_bytes);

        Ok((m1, k_bytes))
    }

    /// Verify M2 = SHA512(BN_bn2bin(A) || M1 || K).
    pub fn verify_server(&self, m1: &[u8], m2: &[u8], k: &[u8]) -> Result<(), SrpError> {
        let a_bytes = self.a_pub.to_bytes_be(); // unpadded
        let expected = sha512(&[a_bytes.as_slice(), m1, k].concat());
        if expected.as_slice() == m2 {
            Ok(())
        } else {
            Err(SrpError::M2Mismatch)
        }
    }
}

// --- Helpers (matching pair_ap naming) ---

fn sha512(data: &[u8]) -> Vec<u8> {
    let mut h = Sha512::new();
    h.update(data);
    h.finalize().to_vec()
}

/// Pad BigUint to N_len bytes (big-endian, leading zeros).
fn padded(n: &BigUint, modulus: &BigUint) -> Vec<u8> {
    let len = (modulus.bits() as usize + 7) / 8;
    let bytes = n.to_bytes_be();
    let mut out = vec![0u8; len.saturating_sub(bytes.len())];
    out.extend_from_slice(&bytes);
    out
}

/// H_nn_pad for k: SHA512(PAD(N, N_len) || PAD(g, N_len))
fn h_nn_pad_ng(n: &BigUint, g: &BigUint) -> BigUint {
    let n_len = (n.bits() as usize + 7) / 8;
    let n_bytes = n.to_bytes_be();
    let g_bytes = g.to_bytes_be();
    let mut data = vec![0u8; n_len * 2];
    data[n_len - n_bytes.len()..n_len].copy_from_slice(&n_bytes);
    data[2 * n_len - g_bytes.len()..].copy_from_slice(&g_bytes);
    BigUint::from_bytes_be(&sha512(&data))
}

/// H_nn_pad for u: SHA512(PAD(a, N_len) || PAD(b, N_len))
fn h_nn_pad(a: &BigUint, b: &BigUint, n: &BigUint) -> BigUint {
    let len = (n.bits() as usize + 7) / 8;
    let mut data = vec![0u8; len * 2];
    let ab = a.to_bytes_be();
    let bb = b.to_bytes_be();
    data[len - ab.len()..len].copy_from_slice(&ab);
    data[2 * len - bb.len()..].copy_from_slice(&bb);
    BigUint::from_bytes_be(&sha512(&data))
}

/// x = SHA512(salt_raw || SHA512(I || ":" || P))
fn compute_x(salt: &[u8], username: &[u8], password: &[u8]) -> BigUint {
    let inner = sha512(&[username, b":", password].concat());
    let outer = sha512(&[salt, &inner].concat());
    BigUint::from_bytes_be(&outer)
}

/// M1 = SHA512(H(N)^H(g) || H(I) || s_unpadded || A_unpadded || B_unpadded || K)
/// All s/A/B via BN_bn2bin (unpadded); K is 64-byte SHA512(S).
fn compute_m1(
    n: &BigUint,
    g: &BigUint,
    username: &[u8],
    salt: &[u8],
    a_pub: &BigUint,
    b_pub: &BigUint,
    k: &[u8],
) -> Vec<u8> {
    let h_n = sha512(&n.to_bytes_be());
    let h_g = sha512(&g.to_bytes_be());
    let xor: Vec<u8> = h_n.iter().zip(h_g.iter()).map(|(a, b)| a ^ b).collect();
    let h_i = sha512(username);
    // update_hash_n → BN_bn2bin = to_bytes_be() (unpadded)
    let s_bytes = salt;               // salt is passed as raw bytes, not a bnum
    let a_bytes = a_pub.to_bytes_be();
    let b_bytes = b_pub.to_bytes_be();
    sha512(&[xor.as_slice(), &h_i, s_bytes, &a_bytes, &b_bytes, k].concat())
}

fn random_private_key(n: &BigUint) -> BigUint {
    let len = (n.bits() as usize + 7) / 8;
    let mut bytes = vec![0u8; len];
    rand::thread_rng().fill_bytes(&mut bytes);
    BigUint::from_bytes_be(&bytes) % (n - BigUint::one()) + BigUint::one()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn n_is_3072_bits() {
        let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap();
        assert_eq!(n.bits(), 3072);
    }

    #[test]
    fn a_pub_padded_is_384_bytes() {
        let c = SrpClient::new(b"Pair-Setup", b"3939");
        assert_eq!(c.a_pub_padded().len(), 384);
    }

    #[test]
    fn two_clients_differ() {
        let c1 = SrpClient::new(b"Pair-Setup", b"3939");
        let c2 = SrpClient::new(b"Pair-Setup", b"3939");
        assert_ne!(c1.a_pub, c2.a_pub);
    }

    /// Full client-server round-trip using pair_ap's exact formulas.
    #[test]
    fn client_server_roundtrip() {
        let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap();
        let g = BigUint::from(5u32);
        let username = b"Pair-Setup";
        let password = b"3939";
        let salt = b"0123456789abcdef"; // 16 bytes, no leading zeros

        let client = SrpClient::new(username, password);
        let a_pub = &client.a_pub;

        // Server: v = g^x mod N
        let x = compute_x(salt, username, password);
        let v = g.modpow(&x, &n);

        // Server: k, b, B = (k*v + g^b) mod N
        let b = random_private_key(&n);
        let k_srv = h_nn_pad_ng(&n, &g);
        let kv = (&k_srv * &v) % &n;
        let gb = g.modpow(&b, &n);
        let b_pub = (&kv + &gb) % &n;

        // Client processes challenge
        let (m1_client, k_client) = client
            .process_challenge(username, password, salt, &b_pub.to_bytes_be())
            .unwrap();

        // Server: u, S, K, expected M1
        let u = h_nn_pad(a_pub, &b_pub, &n);
        let s_srv = (a_pub * v.modpow(&u, &n)).modpow(&b, &n) % &n;
        let k_srv_bytes = sha512(&s_srv.to_bytes_be());
        let m1_expected = compute_m1(&n, &g, username, salt, a_pub, &b_pub, &k_srv_bytes);

        assert_eq!(k_client, k_srv_bytes, "K mismatch");
        assert_eq!(m1_client, m1_expected, "M1 mismatch");

        // Server M2
        let m2 = sha512(&[a_pub.to_bytes_be().as_slice(), &m1_expected, &k_srv_bytes].concat());
        client.verify_server(&m1_client, &m2, &k_client).unwrap();
    }
}

#[cfg(test)]
mod n_integrity {
    use super::*;
    use sha2::Sha256;

    /// Guard against transcription typos in the 3072-bit prime.
    ///
    /// A single hex digit typo (…95581716… instead of …95581718…) once shipped
    /// here: `bits() == 3072` still passed, every self-consistent test passed,
    /// but all SRP values were computed in the wrong group and every real
    /// receiver rejected the proof with kTLVError_Authentication.
    /// SHA-256 fingerprint computed from the canonical RFC 3526 §7 constant
    /// (cross-checked against srptools' PRIME_3072).
    #[test]
    fn n_matches_rfc3526_fingerprint() {
        let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).unwrap();
        let bytes = n.to_bytes_be();
        assert_eq!(bytes.len(), 384);
        let digest = Sha256::digest(&bytes);
        let hex: String = digest.iter().map(|b| format!("{:02x}", b)).collect();
        assert_eq!(
            hex,
            "48cf8b092fbce4359d9871abf74f98e25b6163379eaa15cd9087e800c6d1c55c",
            "N does not match the canonical RFC 3526 3072-bit prime"
        );
    }
}
