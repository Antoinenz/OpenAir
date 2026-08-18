//! Where OpenAir keeps its per-user state.
//!
//! `%APPDATA%\OpenAir` on Windows, `$XDG_CONFIG_HOME/openair` (falling back to
//! `~/.config/openair`) elsewhere. Everything persistent lives here:
//! `pairings.json`, `settings.json`, `ptp_clock_id`, `handoff_restore.txt`.

use std::path::PathBuf;

/// The OpenAir config directory, or `None` when the environment gives us
/// nowhere to put it.
///
/// Returning `None` rather than picking a fallback is deliberate: every caller
/// treats persistence as optional and degrades to in-memory defaults. Writing
/// user state into the current working directory because `HOME` was unset
/// would be worse than not persisting at all.
pub fn config_dir() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|d| d.join(if cfg!(windows) { "OpenAir" } else { "openair" }))
}

/// Path to a named file inside the config directory.
pub fn config_file(name: &str) -> Option<PathBuf> {
    config_dir().map(|d| d.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_file_sits_inside_config_dir() {
        // Skip where the environment provides no home at all.
        let Some(dir) = config_dir() else { return };
        let file = config_file("settings.json").unwrap();
        assert_eq!(file.parent(), Some(dir.as_path()));
        assert_eq!(file.file_name().unwrap(), "settings.json");
    }
}
