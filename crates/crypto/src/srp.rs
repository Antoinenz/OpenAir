/// SRP-6a, 3072-bit group (RFC 5054 Appendix A), SHA-512.
///
/// Used for HomeKit pair-setup. Username is always "Pair-Setup".
/// Transient pairing uses PIN "3939" (hardcoded by the protocol).
///
/// Reference: RFC 5054 §2, ejurgensen/pair_ap, lmcgartland/airplay2-rs.
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
    "DE2BCBF6955817163995497CEA956AE515D2261898FA0510",
    "15728E5A8AAAC42DAD33170D04507A33A85521ABDF1CBA64",
    "ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7",
    "ABF5AE8CDB0933D71E8C94E04A25619DCEE3D2261AD2EE6B",
    "F12FFA06D98A0864D87602733EC86A64521F2B18177B200C",
    "BBE117577A615D6C770988C0BAD946E208E24FA074E5AB31",
    "43DB5BFCE0FD108E4B82D120A93AD2CAFFFFFFFFFFFFFFFF",
);
const G: u32 = 5;

pub struct SrpGroup {
    pub n: BigUint,
    pub g: BigUint,
}

impl SrpGroup {
    pub fn rfc5054_3072() -> Self {
        SrpGroup {
            n: BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("valid N"),
            g: BigUint::from(G),
        }
    }
}

/// Client side of SRP-6a M1–M4 (Transient pairing stops here — no M5/M6).
pub struct SrpClient {
    group: SrpGroup,
    username: Vec<u8>,
    password: Vec<u8>,
    /// Private ephemeral 'a'
    a: BigUint,
    /// Public ephemeral A = g^a mod N
    pub a_pub: BigUint,
}

impl SrpClient {
    /// `username` = "Pair-Setup", `password` = "3939" for Transient pairing.
    pub fn new(username: &[u8], password: &[u8]) -> Self {
        let group = SrpGroup::rfc5054_3072();
        let a = random_private_key(&group.n);
        let a_pub = group.g.modpow(&a, &group.n);
        SrpClient {
            group,
            username: username.to_vec(),
            password: password.to_vec(),
            a,
            a_pub,
        }
    }

    /// Compute M1 and the session key after receiving the server's B and salt.
    ///
    /// Returns `(M1, session_key)` where M1 is the client proof (64 bytes, SHA-512).
    /// Verifies M2 (server proof) via `verify_server`.
    pub fn process_challenge(
        &self,
        salt: &[u8],
        b_pub_bytes: &[u8],
    ) -> Result<(Vec<u8>, Vec<u8>), SrpError> {
        let b_pub = BigUint::from_bytes_be(b_pub_bytes);
        let n = &self.group.n;
        let g = &self.group.g;

        // SRP-6a: u = H(A || B)
        let u = compute_u(&self.a_pub, &b_pub, n);
        if u.is_zero() {
            return Err(SrpError::InvalidU);
        }

        // x = H(salt || H(username || ':' || password))
        let x = compute_x(salt, &self.username, &self.password);

        // v = g^x mod N (verifier, recomputed client-side)
        let v = g.modpow(&x, n);

        // k = H(N || pad(g)) — SRP-6a multiplier
        let k = compute_k(n, g);

        // S = (B - k*v)^(a + u*x) mod N
        let kv = (&k * &v) % n;
        // B - k*v mod N (handle underflow)
        let b_minus_kv = if b_pub >= kv {
            (&b_pub - &kv) % n
        } else {
            (n + &b_pub - &kv) % n
        };
        let exp = &self.a + &u * &x;
        let s = b_minus_kv.modpow(&exp, n);

        let session_key = sha512(&to_padded_bytes(&s, n));

        // M1 = H(H(N) XOR H(g) || H(username) || salt || A || B || K)
        let m1 = compute_m1(n, g, &self.username, salt, &self.a_pub, &b_pub, &session_key);

        Ok((m1, session_key.to_vec()))
    }

    /// Verify the server's M2 proof: H(A || M1 || K).
    pub fn verify_server(&self, m1: &[u8], m2: &[u8], session_key: &[u8]) -> Result<(), SrpError> {
        let a_bytes = to_padded_bytes(&self.a_pub, &self.group.n);
        let expected = sha512(&[a_bytes.as_slice(), m1, session_key].concat());
        if expected.as_slice() == m2 {
            Ok(())
        } else {
            Err(SrpError::M2Mismatch)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SrpError {
    #[error("u = 0 (abort)")]
    InvalidU,
    #[error("server M2 verification failed")]
    M2Mismatch,
}

// --- Helpers ---

fn sha512(data: &[u8]) -> Vec<u8> {
    let mut h = Sha512::new();
    h.update(data);
    h.finalize().to_vec()
}

/// Pad `n` to the byte length of `modulus`, big-endian.
fn to_padded_bytes(n: &BigUint, modulus: &BigUint) -> Vec<u8> {
    let len = (modulus.bits() as usize + 7) / 8;
    let bytes = n.to_bytes_be();
    let mut padded = vec![0u8; len.saturating_sub(bytes.len())];
    padded.extend_from_slice(&bytes);
    padded
}

fn compute_u(a_pub: &BigUint, b_pub: &BigUint, n: &BigUint) -> BigUint {
    let len = (n.bits() as usize + 7) / 8;
    let mut data = vec![0u8; len * 2];
    let a_bytes = a_pub.to_bytes_be();
    let b_bytes = b_pub.to_bytes_be();
    data[len - a_bytes.len()..len].copy_from_slice(&a_bytes);
    data[2 * len - b_bytes.len()..].copy_from_slice(&b_bytes);
    let hash = sha512(&data);
    BigUint::from_bytes_be(&hash) % n
}

fn compute_x(salt: &[u8], username: &[u8], password: &[u8]) -> BigUint {
    let inner = sha512(&[username, b":", password].concat());
    let outer = sha512(&[salt, &inner].concat());
    BigUint::from_bytes_be(&outer)
}

fn compute_k(n: &BigUint, g: &BigUint) -> BigUint {
    let n_len = (n.bits() as usize + 7) / 8;
    let n_bytes = n.to_bytes_be();
    let g_bytes = g.to_bytes_be();
    let mut data = vec![0u8; n_len * 2];
    data[n_len - n_bytes.len()..n_len].copy_from_slice(&n_bytes);
    data[2 * n_len - g_bytes.len()..].copy_from_slice(&g_bytes);
    let hash = sha512(&data);
    BigUint::from_bytes_be(&hash) % n
}

fn compute_m1(
    n: &BigUint,
    g: &BigUint,
    username: &[u8],
    salt: &[u8],
    a_pub: &BigUint,
    b_pub: &BigUint,
    session_key: &[u8],
) -> Vec<u8> {
    let h_n = sha512(&n.to_bytes_be());
    let h_g = sha512(&g.to_bytes_be());
    let xor: Vec<u8> = h_n.iter().zip(h_g.iter()).map(|(a, b)| a ^ b).collect();
    let h_u = sha512(username);
    let a_bytes = to_padded_bytes(a_pub, n);
    let b_bytes = to_padded_bytes(b_pub, n);
    sha512(
        &[xor.as_slice(), &h_u, salt, &a_bytes, &b_bytes, session_key].concat(),
    )
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
    fn group_parses() {
        let g = SrpGroup::rfc5054_3072();
        // N must be 3072 bits
        assert_eq!(g.n.bits(), 3072);
        assert_eq!(g.g, BigUint::from(5u32));
    }

    #[test]
    fn k_multiplier_is_nonzero() {
        let g = SrpGroup::rfc5054_3072();
        let k = compute_k(&g.n, &g.g);
        assert!(!k.is_zero());
    }

    #[test]
    fn a_pub_in_range() {
        let client = SrpClient::new(b"Pair-Setup", b"3939");
        assert!(client.a_pub < client.group.n);
        assert!(!client.a_pub.is_zero());
    }

    /// Smoke test: two clients with different ephemerals produce different A values.
    #[test]
    fn ephemeral_randomness() {
        let c1 = SrpClient::new(b"Pair-Setup", b"3939");
        let c2 = SrpClient::new(b"Pair-Setup", b"3939");
        assert_ne!(c1.a_pub, c2.a_pub);
    }
}
