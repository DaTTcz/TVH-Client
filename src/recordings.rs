//! Recordings (DVR) - app-side layer on top of `src/tvh/mod.rs`'s
//! `DvrEntry`/`dvr_*`/`autorec_*`: a background fetch that gathers
//! everything the Nahrávky tab shows in one round-trip, plus a small
//! background download helper. No disk cache here (unlike `logos.rs`/
//! `epg.rs`) - the lists involved are small and fetching them isn't slow
//! enough to be worth it.

use crate::tvh::{DvrEntry, TvhClient};
use eframe::egui;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::{Duration, Instant};

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// Everything the Nahrávky tab needs, fetched together so the three lists
/// (and the recording-file URL map) all reflect the same moment rather
/// than three/four different ones from separate requests.
#[derive(Clone)]
pub struct RecordingsData {
    pub upcoming: Vec<DvrEntry>,
    pub finished: Vec<DvrEntry>,
    pub failed: Vec<DvrEntry>,
    pub autorec: Vec<serde_json::Value>,
    /// Ticketed, ready-to-play/download URLs, keyed by `DvrEntry::uuid` -
    /// see `TvhClient::dvr_urls`. Missing entries (e.g. if the server
    /// couldn't build the recordings playlist) just mean Play/Stáhnout
    /// aren't available for that row - not a hard failure.
    pub urls: HashMap<String, String>,
}

/// Spawns a background thread that fetches upcoming/finished/failed DVR
/// entries, autorec rules, and the recording-file URL map, sending the
/// combined result (or the first error hit) back over `tx`.
pub fn spawn_fetch(
    ctx: egui::Context,
    url: String,
    user: String,
    password: String,
    tx: Sender<Result<RecordingsData, String>>,
) {
    std::thread::spawn(move || {
        let result = (|| -> Result<RecordingsData, String> {
            let client = TvhClient::with_timeout(&url, &user, &password, FETCH_TIMEOUT)
                .map_err(|e| e.to_string())?;
            let upcoming = client.dvr_upcoming().map_err(|e| e.to_string())?;
            let finished = client.dvr_finished().map_err(|e| e.to_string())?;
            let failed = client.dvr_failed().map_err(|e| e.to_string())?;
            let autorec = client.autorec_list().map_err(|e| e.to_string())?;
            // Best-effort - a server that can't build the recordings M3U
            // for some reason shouldn't block the rest of the tab, it
            // just means Play/Stáhnout won't have anything to work with.
            let urls = client.dvr_urls().unwrap_or_default();
            Ok(RecordingsData { upcoming, finished, failed, autorec, urls })
        })();
        let _ = tx.send(result);
        ctx.request_repaint();
    });
}

/// Shared pause/cancel signal for one running download, checked between
/// each chunk read on the background thread (see `spawn_download`) -
/// `Arc`'d so both the thread and the "⏸"/"✕" buttons in the Nahrávky
/// tab can reach it. There's no real way to "pause" an HTTP transfer
/// mid-flight, so a pause just stops reading/writing further chunks
/// while leaving the connection open (the OS's TCP receive buffer/the
/// server absorbs the backpressure) - resuming just picks the read loop
/// back up. A cancel breaks out of the loop and deletes the partial
/// file.
pub struct DownloadControl {
    paused: AtomicBool,
    cancelled: AtomicBool,
}

impl DownloadControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            paused: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
        })
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }
}

/// One update from a running download, sent over the channel given to
/// [`spawn_download`]. `Progress` is throttled to a few times a second
/// (not sent per chunk) - `Done`/`Cancelled`/`Error` are always the last
/// message sent.
pub enum DownloadUpdate {
    Progress { downloaded: u64, total: Option<u64> },
    Done(PathBuf),
    Cancelled,
    Error(String),
}

/// How often, at most, a `Progress` update is sent - frequent enough for
/// a smooth-looking progress bar, infrequent enough not to flood the
/// channel/repaint on a fast local-network transfer.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(150);

/// Downloads `url` (expected to already be a ready-to-fetch, ticketed URL
/// - see `TvhClient::dvr_urls`) to `dest`, creating parent directories as
/// needed, streaming it in chunks (rather than buffering the whole file
/// in memory first) so progress can be reported and `control` can pause/
/// cancel mid-transfer. On cancel or any error, the partially-written
/// file is removed rather than left behind half-finished.
pub fn spawn_download(
    ctx: egui::Context,
    url: String,
    dest: PathBuf,
    control: Arc<DownloadControl>,
    tx: Sender<DownloadUpdate>,
) {
    std::thread::spawn(move || {
        enum Outcome {
            Done(PathBuf),
            Cancelled,
        }

        let result: Result<Outcome, String> = (|| {
            let client = reqwest::blocking::Client::builder()
                // Recordings can be large and the connection is local-
                // network-speed at best - generous timeout so a big file
                // over a slow link doesn't get killed mid-transfer.
                .timeout(Duration::from_secs(3600))
                .build()
                .map_err(|e| e.to_string())?;
            let mut response = client.get(&url).send().map_err(|e| e.to_string())?;
            if !response.status().is_success() {
                return Err(format!("Server odpověděl chybou {}", response.status()));
            }
            let total = response.content_length();

            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut file = std::fs::File::create(&dest).map_err(|e| e.to_string())?;

            let mut buf = [0u8; 64 * 1024];
            let mut downloaded: u64 = 0;
            let mut last_sent = Instant::now();
            loop {
                if control.is_cancelled() {
                    return Ok(Outcome::Cancelled);
                }
                if control.is_paused() {
                    // Not actually reading from the socket while paused -
                    // just idling the thread and re-checking a few times
                    // a second, so a long pause doesn't spin the CPU.
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                let n = response.read(&mut buf).map_err(|e| e.to_string())?;
                if n == 0 {
                    break;
                }
                file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
                downloaded += n as u64;
                if last_sent.elapsed() >= PROGRESS_INTERVAL {
                    let _ = tx.send(DownloadUpdate::Progress { downloaded, total });
                    ctx.request_repaint();
                    last_sent = Instant::now();
                }
            }
            let _ = tx.send(DownloadUpdate::Progress { downloaded, total });
            Ok(Outcome::Done(dest.clone()))
        })();

        match result {
            Ok(Outcome::Done(path)) => {
                let _ = tx.send(DownloadUpdate::Done(path));
            }
            Ok(Outcome::Cancelled) => {
                let _ = std::fs::remove_file(&dest);
                let _ = tx.send(DownloadUpdate::Cancelled);
            }
            Err(e) => {
                // Best-effort cleanup - a half-written file lying around
                // under a "finished" name would be confusing.
                let _ = std::fs::remove_file(&dest);
                let _ = tx.send(DownloadUpdate::Error(e));
            }
        }
        ctx.request_repaint();
    });
}

/// Suggested default download destination folder - prefilled into the
/// Nastavení > Stahování text field the first time (the user can pick
/// anything else; see `Settings::downloads_dir`). There's no file-save-
/// dialog dependency in this project (yet), so it's a plain path, not a
/// picked-via-dialog one.
pub fn downloads_dir() -> PathBuf {
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return PathBuf::from(profile).join("Videos").join("TVH Client Nahrávky");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join("Videos").join("TVH Client Nahrávky");
    }
    PathBuf::from("TVH Client Nahrávky")
}

/// Where a recording gets temporarily buffered to before mpv ever sees
/// it (Nahrávky tab's "▶ Přehrát") - the OS temp directory, not
/// `downloads_dir`/`Settings::downloads_dir`, since these files are
/// throwaway and deleted again as soon as playback stops or switches to
/// something else (see `TvhApp::clear_recording_playback` in app.rs).
///
/// This buffering step exists because pointing mpv directly at
/// TVHeadend's `dvrfile/<uuid>` URL was observed to just hang - a
/// permanently black video area, no mpv error, no buffering-state signal
/// either - on at least one real connection, even though the exact same
/// URL downloads fine through this same `spawn_download`. Buffering to a
/// local file first and only ever handing mpv that reuses the one
/// download path already proven to work reliably.
pub fn playback_cache_dir() -> PathBuf {
    std::env::temp_dir().join("tvh-client-playback")
}

/// Opens `path` (a file or folder) in the OS file manager and, if it's a
/// file, selects it - Windows-only for now, matching this project's
/// Windows-only scope (see README).
pub fn open_in_file_manager(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer")
            .arg("/select,")
            .arg(path)
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
    }
}

/// Launches `url` (a ready-to-play, ticketed recording URL - see
/// `TvhClient::dvr_urls`) via the OS's default handler for it, the same
/// way double-clicking a link would - mirrors `open_in_file_manager`'s
/// Windows-only, no-new-dependency approach rather than pulling in a
/// crate (e.g. `open`) just for this one call.
pub fn play_url(url: &str) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = url;
    }
}

/// `1234567890` bytes -> `"1.15 GB"`.
pub fn human_size(bytes: i64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes.max(0) as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// A filesystem-safe download filename built from a recording's title,
/// with the extension sniffed from the server-side `filename` field
/// (falls back to `.ts`, TVHeadend's usual recording format).
pub fn safe_filename(title: &str, server_filename: &str) -> String {
    let extension = std::path::Path::new(server_filename)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("ts");

    let cleaned: String = title
        .chars()
        .map(|c| if "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim();
    let base = if trimmed.is_empty() { "nahravka" } else { trimmed };
    format!("{base}.{extension}")
}
