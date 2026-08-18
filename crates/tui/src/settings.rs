//! Persisted global preferences for the TUI.
//!
//! Stored as `settings.json` in the OpenAir config directory, beside
//! `pairings.json`.
//!
//! Precedence is strictly **CLI flag > settings file > built-in default**. A
//! flag overrides for that run only and never rewrites the file; only the
//! TUI's own toggles write it. Selected receivers are deliberately *not*
//! persisted — devices come and go, and silently streaming to a stale
//! selection is worse than picking each time.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

/// Bumped only when a change would confuse an older build. A file carrying a
/// version we do not know is ignored in favour of defaults rather than being
/// guessed at.
const CURRENT_VERSION: u32 = 1;

const FILE_NAME: &str = "settings.json";

pub const LATENCY_MIN_MS: u64 = 100;
pub const LATENCY_MAX_MS: u64 = 2000;
pub const LATENCY_STEP_MS: u64 = 50;

/// Which series the dashboard's graph panel is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum GraphKind {
    /// Milliseconds of headroom before the play deadline — the number that
    /// actually predicts a dropout.
    #[default]
    Buffer,
    /// Encoded bytes per second.
    Bandwidth,
}

impl GraphKind {
    pub fn toggled(self) -> Self {
        match self {
            GraphKind::Buffer => GraphKind::Bandwidth,
            GraphKind::Bandwidth => GraphKind::Buffer,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            GraphKind::Buffer => "buffer health (ms ahead)",
            GraphKind::Bandwidth => "bandwidth",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default = "default_handoff")]
    pub handoff: bool,
    #[serde(default = "default_latency")]
    pub latency_ms: u64,
    #[serde(default = "default_volume")]
    pub volume_db: f32,
    #[serde(default = "default_metadata")]
    pub metadata: bool,
    #[serde(default)]
    pub graph: GraphKind,
}

fn default_version() -> u32 {
    CURRENT_VERSION
}
fn default_handoff() -> bool {
    true
}
fn default_latency() -> u64 {
    500
}
fn default_volume() -> f32 {
    -8.0
}
fn default_metadata() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: CURRENT_VERSION,
            handoff: default_handoff(),
            latency_ms: default_latency(),
            volume_db: default_volume(),
            metadata: default_metadata(),
            graph: GraphKind::default(),
        }
    }
}

impl Settings {
    /// Load from the standard location. Never fails: any problem — missing
    /// file, unreadable, corrupt, from a future version — yields defaults.
    /// Settings are a convenience and must never block a stream.
    pub fn load() -> Self {
        match Self::path() {
            Some(path) => Self::load_from(&path),
            None => Settings::default(),
        }
    }

    pub fn load_from(path: &Path) -> Self {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            // A missing file is the normal first-run case, not a problem.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Settings::default(),
            Err(e) => {
                warn!(path = %path.display(), "could not read settings ({e}) — using defaults");
                return Settings::default();
            }
        };

        let parsed: Settings = match serde_json::from_str(&raw) {
            Ok(parsed) => parsed,
            Err(e) => {
                warn!(path = %path.display(), "settings file is not valid JSON ({e}) — using defaults");
                return Settings::default();
            }
        };

        if parsed.version > CURRENT_VERSION {
            warn!(
                found = parsed.version,
                known = CURRENT_VERSION,
                "settings file is from a newer OpenAir — using defaults"
            );
            return Settings::default();
        }

        parsed.clamped()
    }

    pub fn save(&self) -> std::io::Result<()> {
        match Self::path() {
            Some(path) => self.save_to(&path),
            None => Ok(()),
        }
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(path, json)
    }

    pub fn path() -> Option<PathBuf> {
        openair_core::config::config_file(FILE_NAME)
    }

    /// Pull out-of-range values back into bounds. A hand-edited file should not
    /// be able to start a stream at a latency the pipeline cannot honour.
    fn clamped(mut self) -> Self {
        self.latency_ms = self.latency_ms.clamp(LATENCY_MIN_MS, LATENCY_MAX_MS);
        if !self.volume_db.is_finite() {
            self.volume_db = default_volume();
        }
        self.volume_db = self.volume_db.clamp(-60.0, 0.0);
        self.version = CURRENT_VERSION;
        self
    }

    /// Step the latency by one increment, staying in bounds.
    pub fn nudge_latency(&mut self, up: bool) {
        let next = if up {
            self.latency_ms.saturating_add(LATENCY_STEP_MS)
        } else {
            self.latency_ms.saturating_sub(LATENCY_STEP_MS)
        };
        self.latency_ms = next.clamp(LATENCY_MIN_MS, LATENCY_MAX_MS);
    }
}

/// One run's effective configuration: the file, with any CLI flags laid over
/// it. Flags win, and never write back.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub handoff: Option<bool>,
    pub latency_ms: Option<u64>,
    pub volume_db: Option<f32>,
    pub metadata: Option<bool>,
}

impl Settings {
    pub fn with_overrides(mut self, over: &Overrides) -> Self {
        if let Some(v) = over.handoff {
            self.handoff = v;
        }
        if let Some(v) = over.latency_ms {
            self.latency_ms = v;
        }
        if let Some(v) = over.volume_db {
            self.volume_db = v;
        }
        if let Some(v) = over.metadata {
            self.metadata = v;
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("openair-settings-test-{name}-{}", std::process::id()));
        p.push(FILE_NAME);
        p
    }

    fn cleanup(path: &Path) {
        if let Some(dir) = path.parent() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn missing_file_yields_defaults() {
        let path = temp_path("missing");
        cleanup(&path);
        assert_eq!(Settings::load_from(&path), Settings::default());
    }

    #[test]
    fn round_trips_through_disk() {
        let path = temp_path("roundtrip");
        cleanup(&path);
        let settings = Settings {
            handoff: false,
            latency_ms: 750,
            volume_db: -14.0,
            metadata: false,
            graph: GraphKind::Bandwidth,
            ..Settings::default()
        };

        settings.save_to(&path).unwrap();
        assert_eq!(Settings::load_from(&path), settings);
        cleanup(&path);
    }

    #[test]
    fn handoff_off_survives_a_restart() {
        // The specific behaviour the user asked for: turning handoff off in
        // the picker must still be off next launch.
        let path = temp_path("handoff");
        cleanup(&path);
        let mut settings = Settings::default();
        assert!(settings.handoff, "default is on");
        settings.handoff = false;
        settings.save_to(&path).unwrap();
        assert!(!Settings::load_from(&path).handoff);
        cleanup(&path);
    }

    #[test]
    fn corrupt_file_yields_defaults() {
        let path = temp_path("corrupt");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ this is not json").unwrap();
        assert_eq!(Settings::load_from(&path), Settings::default());
        cleanup(&path);
    }

    #[test]
    fn future_version_yields_defaults() {
        let path = temp_path("future");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"version":999,"latency_ms":1234}"#).unwrap();
        let loaded = Settings::load_from(&path);
        assert_eq!(loaded, Settings::default());
        assert_ne!(loaded.latency_ms, 1234, "must not adopt values we can't vouch for");
        cleanup(&path);
    }

    #[test]
    fn missing_fields_fall_back_per_field() {
        // Forward compatibility the other way: an older file that predates a
        // field should keep its other values rather than resetting wholesale.
        let path = temp_path("partial");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"version":1,"handoff":false}"#).unwrap();
        let loaded = Settings::load_from(&path);
        assert!(!loaded.handoff);
        assert_eq!(loaded.latency_ms, default_latency());
        cleanup(&path);
    }

    #[test]
    fn out_of_range_latency_is_clamped() {
        let path = temp_path("clamp");
        cleanup(&path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"version":1,"latency_ms":999999}"#).unwrap();
        assert_eq!(Settings::load_from(&path).latency_ms, LATENCY_MAX_MS);
        cleanup(&path);
    }

    #[test]
    fn flags_override_file_but_file_overrides_default() {
        let file = Settings {
            latency_ms: 750,
            handoff: false,
            ..Settings::default()
        };

        let effective = file.clone().with_overrides(&Overrides {
            latency_ms: Some(300),
            ..Overrides::default()
        });

        assert_eq!(effective.latency_ms, 300, "flag wins over file");
        assert!(!effective.handoff, "file wins over built-in default");
        assert_eq!(effective.volume_db, default_volume(), "default where neither set");
    }

    #[test]
    fn nudge_latency_clamps_at_both_ends() {
        let mut s = Settings {
            latency_ms: LATENCY_MAX_MS,
            ..Settings::default()
        };
        s.nudge_latency(true);
        assert_eq!(s.latency_ms, LATENCY_MAX_MS);

        s.latency_ms = LATENCY_MIN_MS;
        s.nudge_latency(false);
        assert_eq!(s.latency_ms, LATENCY_MIN_MS);

        s.latency_ms = 500;
        s.nudge_latency(true);
        assert_eq!(s.latency_ms, 500 + LATENCY_STEP_MS);
    }

    #[test]
    fn graph_kind_toggles_and_round_trips() {
        assert_eq!(GraphKind::Buffer.toggled(), GraphKind::Bandwidth);
        assert_eq!(GraphKind::Bandwidth.toggled(), GraphKind::Buffer);
        let json = serde_json::to_string(&GraphKind::Bandwidth).unwrap();
        assert_eq!(json, r#""bandwidth""#);
    }
}
