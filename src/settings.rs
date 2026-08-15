//! Persisted settings: named TVHeadend server profiles + which one is
//! primary (auto-connected at startup).
//!
//! Stored as plain JSON under the OS config directory:
//!   Windows: `%APPDATA%\tvh-client\settings.json`
//!   Linux/macOS (dev only): `~/.config/tvh-client/settings.json`
//!
//! NOTE: passwords are stored **unencrypted**. That's an accepted
//! trade-off for a simple LAN media-server client - don't reuse a
//! sensitive password for your TVHeadend account.
//!
//! Migrates automatically, on first load, from the kolo 1/2 single-server
//! format (`{ "url": ..., "user": ..., "password": ... }`) into a single
//! `ServerProfile` marked primary.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerProfile {
    /// Stable id, independent of `name` - so renaming a server doesn't
    /// break `Settings::primary_id` or `TvhApp::active_server_id`.
    pub id: String,
    pub name: String,
    pub url: String,
    pub user: String,
    pub password: String,
    /// Channel tag uuids to restrict the channel list to (via the "Test"
    /// button in the server form). Empty = show every channel.
    #[serde(default)]
    pub selected_tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub servers: Vec<ServerProfile>,
    #[serde(default)]
    pub primary_id: Option<String>,
    /// Folder recordings get downloaded into (Nahrávky tab's "⬇ Stáhnout"
    /// button, configured in Nastavení > Stahování). Empty means "not set
    /// yet" - the download button then sends the user to that settings
    /// tab instead of asking every time (see `TvhApp::recordings_finished_list`).
    #[serde(default)]
    pub downloads_dir: String,
    /// mpv's volume scale (0-100), restored at startup and saved back
    /// whenever the user changes it (video overlay slider or keyboard
    /// +/-/arrows - see `TvhApp::adjust_volume`) so it doesn't reset to
    /// 100 every time the app is relaunched. Defaults to 100 (mpv's own
    /// default) both for a brand new install and for an old
    /// `settings.json` saved before this field existed - see
    /// `default_volume`/the manual `Default` impl below (`#[derive(Default)]`
    /// would otherwise zero-initialize it, i.e. start muted).
    #[serde(default = "default_volume")]
    pub volume: f64,
}

fn default_volume() -> f64 {
    100.0
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            servers: Vec::new(),
            primary_id: None,
            downloads_dir: String::new(),
            volume: default_volume(),
        }
    }
}

/// Old (kolo 1/2) single-server format - only used to migrate.
#[derive(Debug, Default, Deserialize)]
struct LegacySettings {
    #[serde(default)]
    url: String,
    #[serde(default)]
    user: String,
    #[serde(default)]
    password: String,
}

impl Settings {
    /// Load settings from disk, migrating the old single-server format if
    /// that's what's there. Returns an empty `Settings` if there's
    /// nothing saved yet or the file can't be read/parsed.
    pub fn load() -> Settings {
        let Some(path) = Self::path() else {
            return Settings::default();
        };
        let Ok(content) = std::fs::read_to_string(&path) else {
            return Settings::default();
        };

        let mut settings: Settings = serde_json::from_str(&content).unwrap_or_default();

        if settings.servers.is_empty() {
            if let Ok(legacy) = serde_json::from_str::<LegacySettings>(&content) {
                if !legacy.url.is_empty() || !legacy.user.is_empty() || !legacy.password.is_empty() {
                    let profile = ServerProfile {
                        id: Self::new_id(),
                        name: "Server 1".to_string(),
                        url: legacy.url,
                        user: legacy.user,
                        password: legacy.password,
                        selected_tags: Vec::new(),
                    };
                    settings.primary_id = Some(profile.id.clone());
                    settings.servers.push(profile);
                    // Save right away in the new format, so this
                    // migration only ever has to run once.
                    let _ = settings.save();
                }
            }
        }

        settings
    }

    pub fn save(&self) -> Result<(), String> {
        let path = Self::path().ok_or_else(|| "Nepodařilo se určit konfigurační složku".to_string())?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(&path, json).map_err(|e| e.to_string())
    }

    /// The server to auto-connect to at startup: `primary_id` if it still
    /// points at something, otherwise just the first saved server.
    pub fn primary(&self) -> Option<&ServerProfile> {
        self.primary_id
            .as_deref()
            .and_then(|id| self.servers.iter().find(|s| s.id == id))
            .or_else(|| self.servers.first())
    }

    /// A fresh id for a new server profile - doesn't need to be globally
    /// unique, just unique among what's saved locally.
    pub fn new_id() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        format!("server-{nanos:x}")
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
