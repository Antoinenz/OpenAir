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
    #[error("missing plist field: {0}")]
    MissingPlistField(&'static str),
    #[error("failed to encode plist body")]
    PlistEncode,
    #[error("failed to decode plist response")]
    PlistDecode,
}

/// Connect and Transient-pair, returning the encrypted connection.
pub fn pair(addr: SocketAddr, device_id: &str) -> Result<RtspConnection, SessionError> {
    info!(addr = %addr, "connecting");
    let mut conn = RtspConnection::connect(addr, device_id)?;

    let pairing = TransientPairing::new();

    // --- M1 ---
    info!("pair-setup M1");
    let m1_body = pairing.build_m1();
    let m2_raw = conn.request(
        "POST", "/pair-setup",
        &[("X-Apple-HKP", "4")],
        &m1_body,
        Some("application/octet-stream"),
    )?;
    check_status(&m2_raw, 200)?;
    let m2_body = connection::extract_body(&m2_raw);

    // --- M3 ---
    info!("pair-setup M3 (sending proof)");
    let (m3_body, m1_proof, session_key) = pairing.process_m2_build_m3(m2_body)?;
    let m4_raw = conn.request(
        "POST", "/pair-setup",
        &[("X-Apple-HKP", "4")],
        &m3_body,
        Some("application/octet-stream"),
    )?;
    check_status(&m4_raw, 200)?;
    let m4_body = connection::extract_body(&m4_raw);

    // --- Verify M4, derive keys ---
    info!("verifying M4 and deriving channel keys");
    let keys = pairing.process_m4(m4_body, &m1_proof, &session_key)?;
    conn.enable_encryption(&keys.write, &keys.read);
    info!("encrypted channel established");
    Ok(conn)
}

/// Pair with a device using Transient pairing and send an encrypted GET /info.
/// Returns the raw (decrypted) GET /info response on success.
pub fn pair_and_get_info(
    addr: SocketAddr,
    device_id: &str,
) -> Result<Vec<u8>, SessionError> {
    let mut conn = pair(addr, device_id)?;
    info!("GET /info (encrypted)");
    let info_raw = conn.request("GET", "/info", &[], &[], None)?;
    debug!(bytes = info_raw.len(), "GET /info response received");
    Ok(info_raw)
}

fn check_status(response: &[u8], expected: u16) -> Result<(), SessionError> {
    match connection::status_code(response) {
        Some(code) if code == expected => Ok(()),
        Some(470) => Err(SessionError::AuthorizationRequired),
        Some(code) => Err(SessionError::Http(code)),
        None => Err(SessionError::EmptyResponse),
    }
}
