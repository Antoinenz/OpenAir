/// Full pair-then-GET-/info session.
///
/// This is the first real end-to-end test of the protocol stack:
///   1. TCP connect
///   2. POST /pair-setup M1 (send A)
///   3. POST /pair-setup M3 (send M1 proof, receive M2 proof)
///   4. Enable encrypted channel
///   5. GET /info (encrypted) — if this succeeds, the full crypto stack works
use std::net::SocketAddr;

use openair_pairing::{Identity, NormalPairing, PairVerify, PeerCredentials, TransientPairing};
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

/// One-time Normal pair-setup (X-Apple-HKP: 3) for Apple TV / HomePod.
///
/// POST /pair-pin-start makes the device display a PIN on screen;
/// `pin_provider` is called after M2 so the user can type it in.
/// On success the accessory's long-term identity is returned — persist it
/// and use [`pair_verify`] for all future connections.
pub fn pair_setup_normal(
    addr: SocketAddr,
    device_id: &str,
    identity: &Identity,
    pin_provider: &mut dyn FnMut() -> String,
) -> Result<PeerCredentials, SessionError> {
    info!(addr = %addr, "connecting for Normal pair-setup");
    let mut conn = RtspConnection::connect(addr, device_id)?;
    let hkp = [("X-Apple-HKP", "3")];

    // Trigger the on-screen PIN (pyatv does this before M1; harmless
    // elsewhere — devices without a screen just return 200/404).
    info!("POST /pair-pin-start (PIN should appear on the device)");
    let pin_start = conn.request("POST", "/pair-pin-start", &hkp, &[], None)?;
    debug!(status = ?connection::status_code(&pin_start), "pair-pin-start response");

    let pairing = NormalPairing::new();

    info!("pair-setup M1 (Normal)");
    let m2_raw = conn.request(
        "POST", "/pair-setup",
        &hkp,
        &pairing.build_m1(),
        Some("application/octet-stream"),
    )?;
    check_status(&m2_raw, 200)?;
    let m2_body = connection::extract_body(&m2_raw).to_vec();

    // PIN is on the device's screen now.
    let pin = pin_provider();

    info!("pair-setup M3 (SRP proof with PIN)");
    let (m3_body, m1_proof, srp_key) = pairing.process_m2_build_m3(&m2_body, pin.trim())?;
    let m4_raw = conn.request(
        "POST", "/pair-setup",
        &hkp,
        &m3_body,
        Some("application/octet-stream"),
    )?;
    check_status(&m4_raw, 200)?;
    let m4_body = connection::extract_body(&m4_raw);

    info!("pair-setup M5 (exchanging long-term identities)");
    let (m5_body, encrypt_key) =
        pairing.process_m4_build_m5(m4_body, &m1_proof, &srp_key, identity)?;
    let m6_raw = conn.request(
        "POST", "/pair-setup",
        &hkp,
        &m5_body,
        Some("application/octet-stream"),
    )?;
    check_status(&m6_raw, 200)?;
    let m6_body = connection::extract_body(&m6_raw);

    let peer = pairing.process_m6(m6_body, &srp_key, &encrypt_key)?;
    info!("pair-setup complete — accessory identity verified");
    Ok(peer)
}

/// Per-connection pair-verify (X-Apple-HKP: 3) using stored credentials.
/// Returns the encrypted connection, like [`pair`] does for Transient.
pub fn pair_verify(
    addr: SocketAddr,
    device_id: &str,
    identity: Identity,
    peer: PeerCredentials,
) -> Result<RtspConnection, SessionError> {
    info!(addr = %addr, "connecting (pair-verify)");
    let mut conn = RtspConnection::connect(addr, device_id)?;
    let hkp = [("X-Apple-HKP", "3")];
    let mut pv = PairVerify::new(identity, peer);

    info!("pair-verify M1");
    let m2_raw = conn.request(
        "POST", "/pair-verify",
        &hkp,
        &pv.build_m1(),
        Some("application/octet-stream"),
    )?;
    check_status(&m2_raw, 200)?;

    info!("pair-verify M3");
    let m3_body = pv.process_m2_build_m3(connection::extract_body(&m2_raw))?;
    let m4_raw = conn.request(
        "POST", "/pair-verify",
        &hkp,
        &m3_body,
        Some("application/octet-stream"),
    )?;
    check_status(&m4_raw, 200)?;

    let keys = pv.process_m4(connection::extract_body(&m4_raw))?;
    conn.enable_encryption(&keys.write, &keys.read);
    info!("encrypted channel established (pair-verify)");
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
