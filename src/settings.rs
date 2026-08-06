//! Persisted connection settings (server URL, user, password).
//!
//! Stored as plain JSON under the OS config directory:
//!   Windows: `%APPDATA%\tvh-client\settings.json`
//!   Linux/macOS (dev only): `~/.config/tvh-client/settings.json`
//!
//! NOTE: the password is stored **unencrypted**. That's an accepted
//! trade-off for a simple LAN media-server client - don't reuse a
//! sensitive password for your TVHeadend account.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub user: String,
    #[serde(default)]
    pub password: String,
}

impl Settings {
    /// Load previously saved settings, or defaults if none exist / the
    /// file can't be read or parsed.
    pub fn load() -> Settings {
        let Some(path) = Self::path() else {
            return Settings::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    }

    /// Save to disk, creating the config directory if it doesn't exist yet.
    pub fn save(&self) -> Result<(), String> {
        let path = Self::path().ok_or_else(|| "Nepodařilo se určit konfigurační složku".to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| e.to_string())
    }

    /// Delete any saved settings file (no-op if there isn't one).
    pub fn clear() -> Result<(), String> {
        let Some(path) = Self::path() else {
            return Ok(());
        };
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    fn path() -> Option<PathBuf> {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return Some(PathBuf::from(appdata).join("tvh-client").join("settings.json"));
        }
        if let Some(home) = std::env::var_os("HOME") {
            return Some(
                PathBuf::from(home)
                    .join(".config")
                    .join("tvh-client")
                    .join("settings.json"),
            );
        }
        None
    }
}
