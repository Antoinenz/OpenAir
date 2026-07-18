//! Persistent HomeKit pairing store.
//!
//! One JSON file holds our long-term controller identity (pairing ID +
//! Ed25519 seed) and, per receiver device-id, the accessory's long-term
//! identity learned during Normal pair-setup:
//!
//! ```json
//! {
//!   "pairing_id": "5f8de963-....",
//!   "ltsk": "<hex 32 bytes>",
//!   "peers": {
//!     "AA:BB:CC:DD:EE:FF": { "peer_id": "<hex>", "ltpk": "<hex 32 bytes>" }
//!   }
//! }
//! ```
//!
//! Location: `%APPDATA%\OpenAir\pairings.json` on Windows,
//! `$XDG_CONFIG_HOME/openair/pairings.json` (or `~/.config/...`) elsewhere.
use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use openair_pairing::{Identity, PeerCredentials};
use serde::{Deserialize, Serialize};
use tracing::debug;

#[derive(Serialize, Deserialize, Default)]
struct StoreFile {
    pairing_id: String,
    /// Ed25519 long-term secret seed, hex.
    ltsk: String,
    #[serde(default)]
    peers: BTreeMap<String, PeerEntry>,
}

#[derive(Serialize, Deserialize, Clone)]
struct PeerEntry {
    /// Accessory pairing identifier bytes, hex (may be non-UTF-8 in theory).
    peer_id: String,
    /// Accessory Ed25519 long-term public key, hex.
    ltpk: String,
}

/// The on-disk pairing store, loaded into memory.
pub struct PairingStore {
    path: PathBuf,
    file: StoreFile,
}

impl PairingStore {
    /// Load the store, creating a fresh identity (and parent directory)
    /// on first use.
    pub fn load() -> io::Result<Self> {
        let path = store_path()?;
        let file = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("corrupt pairing store {}: {e}", path.display()),
                )
            })?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let identity = Identity::generate();
                debug!(path = %path.display(), "creating new pairing store");
                StoreFile {
                    pairing_id: String::from_utf8_lossy(&identity.pairing_id).into_owned(),
                    ltsk: hex_encode(&identity.signing_seed),
                    peers: BTreeMap::new(),
                }
            }
            Err(e) => return Err(e),
        };
        Ok(PairingStore { path, file })
    }

    /// Our long-term controller identity.
    pub fn identity(&self) -> io::Result<Identity> {
        let seed: [u8; 32] = hex_decode(&self.file.ltsk)
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "corrupt ltsk in pairing store")
            })?;
        Ok(Identity {
            pairing_id: self.file.pairing_id.clone().into_bytes(),
            signing_seed: seed,
        })
    }

    /// Stored accessory credentials for a receiver device-id, if paired.
    pub fn peer(&self, device_id: &str) -> Option<PeerCredentials> {
        let entry = self.file.peers.get(device_id)?;
        let peer_id = hex_decode(&entry.peer_id)?;
        let ltpk: [u8; 32] = hex_decode(&entry.ltpk)?.try_into().ok()?;
        Some(PeerCredentials { peer_id, ltpk })
    }

    /// Record (or replace) the accessory credentials for a device and save.
    pub fn set_peer(&mut self, device_id: &str, peer: &PeerCredentials) -> io::Result<()> {
        self.file.peers.insert(
            device_id.to_string(),
            PeerEntry {
                peer_id: hex_encode(&peer.peer_id),
                ltpk: hex_encode(&peer.ltpk),
            },
        );
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string_pretty(&self.file).map_err(io::Error::other)?;
        std::fs::write(&self.path, text)
    }

    /// Persist the store even if no peer was added yet (e.g. to pin the
    /// freshly generated identity before pair-setup starts).
    pub fn ensure_saved(&self) -> io::Result<()> {
        if self.path.exists() {
            return Ok(());
        }
        self.save()
    }
}

fn store_path() -> io::Result<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|p| p.join("OpenAir"))
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .map(|p| p.join("openair"))
    };
    base.map(|p| p.join("pairings.json")).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "cannot locate config directory (APPDATA/HOME unset)",
        )
    })
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let data = [0x00u8, 0xFF, 0x5A, 0x01];
        assert_eq!(hex_decode(&hex_encode(&data)).unwrap(), data);
        assert!(hex_decode("abc").is_none()); // odd length
        assert!(hex_decode("zz").is_none()); // invalid digit
    }

    #[test]
    fn store_file_json_roundtrip() {
        let mut peers = BTreeMap::new();
        peers.insert(
            "AA:BB".to_string(),
            PeerEntry {
                peer_id: hex_encode(b"acc-id"),
                ltpk: hex_encode(&[9u8; 32]),
            },
        );
        let f = StoreFile {
            pairing_id: "uuid-here".into(),
            ltsk: hex_encode(&[7u8; 32]),
            peers,
        };
        let text = serde_json::to_string(&f).unwrap();
        let back: StoreFile = serde_json::from_str(&text).unwrap();
        assert_eq!(back.pairing_id, "uuid-here");
        assert_eq!(back.peers["AA:BB"].ltpk, hex_encode(&[9u8; 32]));
    }
}
