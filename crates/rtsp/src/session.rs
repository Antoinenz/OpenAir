/// Full pair-then-GET-/info session.
///
/// This is the first real end-to-end test of the protocol stack:
///   1. TCP connect
///   2. POST /pair-setup M1 (send A)
///   3. POST /pair-setup M3 (send M1 proof, receive M2 proof)
///   4. Enable encrypted channel
///   5. GET /info (encrypted) — if this succeeds, the full crypto stack works
use std::net::SocketAddr;

use openair_pairing::TransientPairing;
use thiserror::Error;
use tracing::{debug, info};

use crate::connection::{self, RtspConnection};

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("TCP error: {0}")]
    Io(#[from] std::io::Error),
    #[error("pairing failed: {0}")]
    Pairing(#[from] openair_pairing::PairingError),
    /// HTTP 470 — device requires explicit user authorization (Apple TV access control).
    /// User must approve the connection on the device screen, or check Home app
    /// "Speakers & TV Access" setting.
    #[error("device requires user authorization (HTTP 470) — approve on the device screen")]
    AuthorizationRequired,
    #[error("unexpected HTTP status {0}")]
    Http(u16),
    #[error("empty response")]
    EmptyResponse,
}

/// Pair with a device using Transient pairing and send an encrypted GET /info.
/// Returns the raw (decrypted) GET /info response body on success.
pub fn pair_and_get_info(
    addr: SocketAddr,
    device_id: &str,
) -> Result<Vec<u8>, SessionError> {
    info!(addr = %addr, "connecting");
    let mut conn = RtspConnection::connect(addr, device_id)?;

    let pairing = TransientPairing::new();

    // --- M1: send A ---
    info!("pair-setup M1 (sending A)");
    let m1_body = pairing.build_m1();
    debug!(m1_len = m1_body.len(), m1_hex = %hex(&m1_body[..m1_body.len().min(32)]), "M1 body (first 32 bytes)");
    let m2_raw = conn.request(
        "POST", "/pair-setup",
        &[("X-Apple-HKP", "4")],
        &m1_body,
        Some("application/pairing+tlv8"),
    )?;
    debug!(bytes = m2_raw.len(), "M2 received");

    check_status(&m2_raw, 200)?;
    let m2_body = connection::extract_body(&m2_raw);
    debug!(body_len = m2_body.len(), body_hex = %hex(m2_body), "M2 body");

    // --- M3: process M2, send proof ---
    info!("pair-setup M3 (sending proof)");
    let (m3_body, m1_proof, session_key) = pairing.process_m2_build_m3(m2_body)?;
    let m4_raw = conn.request(
        "POST", "/pair-setup",
        &[("X-Apple-HKP", "4")],
        &m3_body,
        Some("application/pairing+tlv8"),
    )?;
    debug!(bytes = m4_raw.len(), "M4 received");

    debug!(bytes = m4_raw.len(), m4_hex = %hex(connection::extract_body(&m4_raw)), "M4 body");
    check_status(&m4_raw, 200)?;
    let m4_body = connection::extract_body(&m4_raw);

    // --- Verify M4, derive keys ---
    info!("verifying M4 and deriving channel keys");
    let keys = pairing.process_m4(m4_body, &m1_proof, &session_key)?;
    conn.enable_encryption(&keys.write, &keys.read);
    info!("encrypted channel established");

    // --- Encrypted GET /info ---
    info!("GET /info (encrypted)");
    let info_raw = conn.request("GET", "/info", &[], &[], None)?;
    debug!(bytes = info_raw.len(), "GET /info response received");

    Ok(info_raw)
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ")
}

fn check_status(response: &[u8], expected: u16) -> Result<(), SessionError> {
    match connection::status_code(response) {
        Some(code) if code == expected => Ok(()),
        Some(470) => Err(SessionError::AuthorizationRequired),
        Some(code) => Err(SessionError::Http(code)),
        None => Err(SessionError::EmptyResponse),
    }
}
