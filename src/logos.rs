//! Download, per-server disk cache, and decoding of channel logos.
//!
//! TVHeadend playlist logo URLs (`tvg-logo`/`logo` EXTINF attribute, see
//! `tvh/m3u.rs`) look like:
//!
//!     http://host:9981/imagecache/7167?auth=<token>
//!
//! The `auth=` token can differ between requests even for the exact same
//! image, so we use the URL's *path* (without the query string) as the
//! cache key - that's what actually identifies a specific logo on the
//! server. This also means no digest-auth handling is needed to fetch
//! these: the token in the query string is already a complete credential,
//! same as with stream URLs.
//!
//! Cache layout: `<config dir>/logos/<server_id>/<hash of path>` (no file
//! extension - `image::load_from_memory` detects the format from the
//! file's contents, not its name).
//!
//! Loading strategy per channel: show the cached copy immediately if
//! there is one (no network wait), then always re-fetch in the
//! background and, if the bytes actually changed (or nothing was
//! cached), write the new copy to disk and push an updated texture.

use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::Sender;

/// Cache key for a channel's logo: the URL path without the query
/// string. Stable across requests/sessions even though the `auth=` token
/// in the full URL isn't.
pub fn cache_key(logo_url: &str) -> Option<String> {
    let path = logo_url.split('?').next()?;
    if path.is_empty() {
        None
    } else {
        Some(path.to_string())
    }
}

fn config_dir() -> Option<PathBuf> {
    if let Ok(appdata) = std::env::var("APPDATA") {
        return Some(PathBuf::from(appdata).join("tvh-client"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("tvh-client"))
}

fn sanitize_for_path(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

/// Small, dependency-free FNV-1a hash - no cryptographic properties
/// needed here, just a stable, filesystem-safe filename per cache key.
fn hash_key(key: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in key.bytes() {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn cache_file(server_id: &str, key: &str) -> Option<PathBuf> {
    let dir = config_dir()?.join("logos").join(sanitize_for_path(server_id));
    Some(dir.join(format!("{:016x}", hash_key(key))))
}

/// Spawns one background thread that syncs logos for `channels`
/// (`(cache_key, logo_url)` pairs) belonging to `server_id`, sending
/// `(cache_key, texture)` through `tx` as they become available.
///
/// Runs in two passes so cached logos show up as fast as possible:
///
/// 1. First, every channel's cached copy (if any) is read from disk and
///    sent - pure disk I/O, no network, so this finishes for the whole
///    list almost instantly. (Previously this was interleaved with pass
///    2 below - each channel's cache read had to wait behind every
///    *earlier* channel's blocking network round-trip, which is why
///    icons appeared to trickle in one by one even on a second launch
///    with everything already cached.)
/// 2. Then, each channel is checked against the server in the
///    background; if the logo actually changed (or nothing was cached),
///    the new copy is written to disk and pushed as an update.
///
/// Dropping the receiving end of `tx` (e.g. because the app switched to a
/// different server) makes the thread stop early on its next send.
pub fn spawn_logo_sync(
    ctx: egui::Context,
    server_id: String,
    channels: Vec<(String, String)>,
    tx: Sender<(String, egui::TextureHandle)>,
) {
    std::thread::spawn(move || {
        // Pass 1: cached copies only, no network.
        let mut cached_bytes: Vec<Option<Vec<u8>>> = Vec::with_capacity(channels.len());
        for (key, _url) in &channels {
            let bytes = cache_file(&server_id, key).and_then(|p| std::fs::read(p).ok());
            if let Some(bytes) = &bytes {
                if let Some(handle) = decode_and_load(&ctx, key, bytes) {
                    if tx.send((key.clone(), handle)).is_err() {
                        return;
                    }
                }
            }
            cached_bytes.push(bytes);
        }
        ctx.request_repaint();

        // Pass 2: check the server for changes, background-refresh
        // anything that differs (or wasn't cached at all).
        let Ok(client) = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(8))
            .build()
        else {
            return;
        };

        for ((key, url), cached) in channels.iter().zip(cached_bytes.into_iter()) {
            let Ok(resp) = client.get(url).send() else {
                continue;
            };
            if !resp.status().is_success() {
                continue;
            }
            let Ok(fresh) = resp.bytes() else {
                continue;
            };
            let fresh = fresh.to_vec();

            if cached.as_deref() == Some(fresh.as_slice()) {
                continue; // Unchanged - already showing it from cache (pass 1).
            }

            if let Some(path) = cache_file(&server_id, key) {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let _ = std::fs::write(&path, &fresh);
            }

            if let Some(handle) = decode_and_load(&ctx, key, &fresh) {
                if tx.send((key.clone(), handle)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        }
    });
}

/// Decodes an in-memory image (PNG/etc, via the `image` crate) into an
/// egui texture. `pub(crate)` so `app.rs` can reuse it for the "O
/// programu" logo instead of duplicating the decode dance.
pub(crate) fn decode_and_load(ctx: &egui::Context, key: &str, bytes: &[u8]) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let color_image = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw());
    Some(ctx.load_texture(key, color_image, egui::TextureOptions::LINEAR))
}
