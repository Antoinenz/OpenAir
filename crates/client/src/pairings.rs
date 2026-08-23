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
//!     "AA:BB:CC:DD:EE:FF": {
//!       "peer_id": "<hex>",
//!       "ltpk": "<hex 32 bytes>",
//!       "name": "Living Room"
//!     }
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
    /// What the receiver called itself when we paired with it.
    ///
    /// Cosmetic — nothing in the protocol uses it — but without it the only
    /// handle a person has on a stored pairing is a MAC address, which makes
    /// "forget the one in the pool room" an exercise in guessing. Optional
    /// because stores written before this existed have none, and a missing
    /// name must not make the file unreadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

/// A receiver we hold credentials for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedPeer {
    pub device_id: String,
    /// The name recorded at pairing time, where there is one.
    pub name: Option<String>,
}

impl PairedPeer {
    /// What to put in front of a person: the name if we have one, and the
    /// device id if we do not, rather than nothing at all.
    pub fn label(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.device_id)
    }
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

    /// Device IDs we hold credentials for.
    ///
    /// The picker uses this to mark receivers as already paired without
    /// contacting anything — the whole point being that it can show useful
    /// state from local knowledge alone.
    pub fn peer_ids(&self) -> Vec<String> {
        self.file.peers.keys().cloned().collect()
    }

    /// Everything we are paired with, in a stable order.
    ///
    /// Sorted by device id because that is the map's key order, so the list a
    /// user is looking at does not rearrange itself between openings.
    pub fn peers(&self) -> Vec<PairedPeer> {
        self.file
            .peers
            .iter()
            .map(|(device_id, entry)| PairedPeer {
                device_id: device_id.clone(),
                name: entry.name.clone(),
            })
            .collect()
    }

    /// Record (or replace) the accessory credentials for a device and save.
    ///
    /// `name` is what the receiver was called at the time. Re-pairing an
    /// existing device updates it, so a renamed speaker stops showing its old
    /// name the next time it is paired.
    pub fn set_peer(
        &mut self,
        device_id: &str,
        peer: &PeerCredentials,
        name: Option<&str>,
    ) -> io::Result<()> {
        self.file.peers.insert(
            device_id.to_string(),
            PeerEntry {
                peer_id: hex_encode(&peer.peer_id),
                ltpk: hex_encode(&peer.ltpk),
                name: name.map(str::to_string),
            },
        );
        self.save()
    }

    /// Drop the credentials for a device and save. `false` if there were none.
    ///
    /// Only our half of the pairing goes: the receiver keeps its record of us
    /// until it is told otherwise or reset. That asymmetry is harmless —
    /// pairing again simply replaces the accessory's entry — but it is why
    /// this is "forget" and not "unpair".
    pub fn forget(&mut self, device_id: &str) -> io::Result<bool> {
        if self.file.peers.remove(device_id).is_none() {
            return Ok(false);
        }
        self.save()?;
        Ok(true)
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
    openair_core::config::config_file("pairings.json").ok_or_else(|| {
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

    fn entry(name: Option<&str>) -> PeerEntry {
        PeerEntry {
            peer_id: hex_encode(b"acc-id"),
            ltpk: hex_encode(&[9u8; 32]),
            name: name.map(str::to_string),
        }
    }

    /// A store with no file behind it. `save` would fail; nothing here calls it.
    fn store(peers: BTreeMap<String, PeerEntry>) -> PairingStore {
        PairingStore {
            path: PathBuf::from("unused"),
            file: StoreFile {
                pairing_id: "uuid-here".into(),
                ltsk: hex_encode(&[7u8; 32]),
                peers,
            },
        }
    }

    #[test]
    fn store_file_json_roundtrip() {
        let mut peers = BTreeMap::new();
        peers.insert("AA:BB".to_string(), entry(Some("Living Room")));
        let f = StoreFile {
            pairing_id: "uuid-here".into(),
            ltsk: hex_encode(&[7u8; 32]),
            peers,
        };
        let text = serde_json::to_string(&f).unwrap();
        let back: StoreFile = serde_json::from_str(&text).unwrap();
        assert_eq!(back.pairing_id, "uuid-here");
        assert_eq!(back.peers["AA:BB"].ltpk, hex_encode(&[9u8; 32]));
        assert_eq!(back.peers["AA:BB"].name.as_deref(), Some("Living Room"));
    }

    #[test]
    fn a_store_written_before_names_existed_still_loads() {
        // Real files on real machines predate the name field. Refusing to read
        // one would lose every pairing the user already has.
        let text = r#"{
            "pairing_id": "uuid-here",
            "ltsk": "00",
            "peers": { "AA:BB": { "peer_id": "6163632d6964", "ltpk": "09" } }
        }"#;
        let back: StoreFile = serde_json::from_str(text).unwrap();
        assert!(back.peers["AA:BB"].name.is_none());
    }

    #[test]
    fn a_nameless_entry_is_not_written_back_as_null() {
        // Round-tripping an old file should leave it looking like an old file,
        // not sprinkle `"name": null` through something a person may edit.
        let mut peers = BTreeMap::new();
        peers.insert("AA:BB".to_string(), entry(None));
        let text = serde_json::to_string(&StoreFile {
            pairing_id: "u".into(),
            ltsk: "00".into(),
            peers,
        })
        .unwrap();
        assert!(!text.contains("name"), "{text}");
    }

    #[test]
    fn peers_are_listed_with_their_names() {
        let mut peers = BTreeMap::new();
        peers.insert("BB:BB".to_string(), entry(Some("Pool Room")));
        peers.insert("AA:AA".to_string(), entry(None));
        let listed = store(peers).peers();

        // Sorted by device id: the list must not rearrange between openings.
        assert_eq!(listed[0].device_id, "AA:AA");
        assert_eq!(listed[1].device_id, "BB:BB");
        assert_eq!(listed[1].name.as_deref(), Some("Pool Room"));
    }

    #[test]
    fn a_peer_with_no_name_is_labelled_by_its_device_id() {
        // Showing an empty row would make an old pairing impossible to select,
        // and therefore impossible to forget.
        let anonymous = PairedPeer {
            device_id: "AA:BB:CC:DD:EE:FF".into(),
            name: None,
        };
        assert_eq!(anonymous.label(), "AA:BB:CC:DD:EE:FF");

        let named = PairedPeer {
            device_id: "AA:BB:CC:DD:EE:FF".into(),
            name: Some("Living Room".into()),
        };
        assert_eq!(named.label(), "Living Room");
    }

    #[test]
    fn setting_and_forgetting_a_peer_reaches_the_file() {
        // Through a real file, because the point of forgetting is that it
        // survives a restart -- an in-memory removal that never lands on disk
        // would look identical to every assertion that stopped short of one.
        let path =
            std::env::temp_dir().join(format!("openair-pairings-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let mut s = PairingStore {
            path: path.clone(),
            file: StoreFile {
                pairing_id: "uuid-here".into(),
                ltsk: hex_encode(&[7u8; 32]),
                peers: BTreeMap::new(),
            },
        };
        let creds = PeerCredentials {
            peer_id: b"acc-id".to_vec(),
            ltpk: [9u8; 32],
        };

        s.set_peer("AA:BB", &creds, Some("Pool Room")).unwrap();
        let on_disk: StoreFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk.peers["AA:BB"].name.as_deref(), Some("Pool Room"));

        assert!(s.forget("AA:BB").unwrap(), "it was there to forget");
        let on_disk: StoreFile =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(on_disk.peers.is_empty(), "still on disk after forgetting");

        // Our own identity must survive: forgetting one receiver is not the
        // same as starting again, and losing the controller key would strand
        // every *other* accessory that has stored it.
        assert_eq!(on_disk.pairing_id, "uuid-here");
        assert_eq!(on_disk.ltsk, hex_encode(&[7u8; 32]));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn forgetting_an_unknown_device_is_not_an_error() {
        // It reports `false` rather than failing: the caller asked for a state
        // that already holds, and there is no file write to fail at.
        let mut s = store(BTreeMap::new());
        assert!(!s.forget("AA:BB").unwrap());
    }
}
