//! EPG (programme guide) data: fetch the full grid from TVHeadend, cache
//! it to disk per server, and provide now/next lookups per channel.
//!
//! `api/epg/events/grid` can be *big* (TVHeadend's own docs show a
//! real-world example with a `totalCount` of 18575) and slow for
//! TVHeadend itself to build on modest hardware, so instead of one giant
//! request this fetches it page by page (sorted by start time - the
//! server default), sending the growing accumulated list to the UI after
//! *every* page. Since events sorted by start means the earliest pages
//! are the soonest-to-air ones (TVHeadend prunes fully-elapsed entries),
//! this means "what's on now" for every channel typically shows up
//! within the first page or two, well before the rest of the guide has
//! finished downloading - rather than the UI staying blank until
//! everything arrives.
//!
//! There's still just one cache file per server
//! (`<config dir>/epg/<server_id>.json`, the fully-accumulated result),
//! using the same "show the cached copy instantly, then refresh from the
//! network" strategy as `src/logos.rs`.

use crate::tvh::{EpgEvent, TvhClient};
use eframe::egui;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::time::Duration;

/// Rows per page. Small enough that even a slow server should answer
/// well within `PAGE_TIMEOUT`, large enough that a big guide doesn't
/// need hundreds of round-trips.
const PAGE_SIZE: usize = 4000;

/// Per-page request timeout. Generous (a single-page request is a lot
/// lighter than the old one-shot full-grid fetch), but still bounded so
/// a stuck request eventually surfaces as an error instead of hanging
/// forever.
const PAGE_TIMEOUT: Duration = Duration::from_secs(60);

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

fn cache_file(server_id: &str) -> Option<PathBuf> {
    Some(
        config_dir()?
            .join("epg")
            .join(format!("{}.json", sanitize_for_path(server_id))),
    )
}

fn load_cached(server_id: &str) -> Option<Vec<EpgEvent>> {
    let path = cache_file(server_id)?;
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn save_cache(server_id: &str, events: &[EpgEvent]) {
    let Some(path) = cache_file(server_id) else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_vec(events) {
        let _ = std::fs::write(path, json);
    }
}

/// Groups a flat event list into a per-channel map, each channel's events
/// sorted by start time - the shape the UI actually wants for now/next
/// lookups.
pub fn group_by_channel(events: Vec<EpgEvent>) -> HashMap<String, Vec<EpgEvent>> {
    let mut map: HashMap<String, Vec<EpgEvent>> = HashMap::new();
    for event in events {
        map.entry(event.channel_uuid.clone()).or_default().push(event);
    }
    for events in map.values_mut() {
        events.sort_by_key(|e| e.start);
    }
    map
}

/// The event covering `now` (if any), and the one right after it in the
/// schedule (if any) - `events` must already be sorted by start (as
/// `group_by_channel` leaves them).
pub fn current_and_next(events: &[EpgEvent], now: i64) -> (Option<&EpgEvent>, Option<&EpgEvent>) {
    match events.iter().position(|e| e.start <= now && now < e.stop) {
        Some(i) => (Some(&events[i]), events.get(i + 1)),
        // Gap in the schedule (or nothing loaded yet for this channel) -
        // "next" is just the first upcoming event, if any.
        None => (None, events.iter().find(|e| e.start >= now)),
    }
}

/// How far into `ev` `now` is: `0.0` (just started) to `1.0` (about to
/// end, or already over) - for a progress bar next to "what's playing".
pub fn progress_fraction(ev: &EpgEvent, now: i64) -> f32 {
    let span = (ev.stop - ev.start).max(1) as f32;
    let elapsed = (now - ev.start) as f32;
    (elapsed / span).clamp(0.0, 1.0)
}

/// Single-line (or short, no-newline) tags that some EPG sources tack on
/// as their own lines inside `EpgEvent::description` - accessibility/
/// technical flags, not part of the actual synopsis. Matched case-
/// insensitively against a trimmed line. Not exhaustive, just what's been
/// observed in the wild; extend as new junk shows up.
const EPG_JUNK_LINES: &[&str] = &[
    "hdtv",
    "hd",
    "sd",
    "zvukový popis",
    "zvukovy popis",
    "audio description",
    "skryté titulky",
    "skryte titulky",
    "teletext",
    "stereo",
    "mono",
    "dolby digital",
    "5.1",
    "širokoúhlý",
    "sirokouhly",
    "širokoúhlé",
    "16:9",
    "4:3",
    "premiéra",
    "premiera",
    "repríza",
    "repriza",
];

/// The best synopsis text we can put together for `ev`, for display in the
/// channel list's hover tooltip and the video info overlay.
///
/// `summary` (DVB's "short event descriptor") is often hard-truncated by
/// the broadcaster mid-sentence - on the order of ~170 bytes, a limit
/// baked into the DVB spec, not something TVHeadend or we control. The
/// *start* of `description` (the "extended event descriptor") is typically
/// the direct continuation of that same sentence, so we glue them
/// together. Some sources then follow that continuation with a block of
/// one-line technical/accessibility tags (`EPG_JUNK_LINES`) and sometimes
/// even a second, differently-phrased synopsis after that - we stop at
/// the first junk line and drop everything from there on, since that
/// belongs in a future full-EPG view, not a compact tooltip/overlay.
pub fn synopsis(ev: &EpgEvent) -> Option<String> {
    let summary = ev.summary.as_deref().map(str::trim).filter(|s| !s.is_empty());

    let description_head = ev.description.as_deref().and_then(|d| {
        let head: Vec<&str> = d
            .lines()
            .take_while(|line| {
                let normalized = line.trim().to_lowercase();
                !EPG_JUNK_LINES.contains(&normalized.as_str())
            })
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        if head.is_empty() {
            None
        } else {
            Some(head.join(" "))
        }
    });

    match (summary, description_head) {
        (Some(s), Some(d)) => Some(format!("{s} {d}")),
        (Some(s), None) => Some(s.to_string()),
        (None, Some(d)) => Some(d),
        (None, None) => None,
    }
}

/// Current time as a unix timestamp (seconds) - `EpgEvent::start`/`stop`
/// are in the same units, so this is what `current_and_next` wants for
/// `now`.
pub fn now_unix() -> i64 {
    chrono::Local::now().timestamp()
}

/// Formats a unix timestamp as `HH:MM` in the system's local timezone.
pub fn format_time(unix: i64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(unix, 0)
        .single()
        .map(|dt| dt.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// One progress update from [`spawn_epg_sync`]: everything fetched *so
/// far* (already the complete list, not just the latest page - the UI
/// just replaces what it had each time) plus `(loaded, total)` for a
/// progress indicator. `loaded == total` means this batch is complete
/// (either the cache, or the last page of a fresh fetch).
pub struct EpgProgress {
    pub events: Vec<EpgEvent>,
    pub loaded: usize,
    pub total: usize,
}

/// Spawns a background thread that: sends this server's cached EPG (if
/// any) immediately, then fetches a fresh copy from the server page by
/// page (see module docs), sending the growing accumulated list after
/// every page so the UI can start showing "what's on now" well before
/// the whole guide has downloaded. Saves the complete result to disk once
/// done. Sends `Err` if a page fetch fails (wrong credentials, timeout,
/// ...) - whatever was already accumulated/sent stays visible, only the
/// error banner appears alongside it.
pub fn spawn_epg_sync(
    ctx: egui::Context,
    server_id: String,
    url: String,
    user: String,
    password: String,
    tx: Sender<Result<EpgProgress, String>>,
) {
    std::thread::spawn(move || {
        if let Some(cached) = load_cached(&server_id) {
            let loaded = cached.len();
            if tx
                .send(Ok(EpgProgress { events: cached, loaded, total: loaded }))
                .is_err()
            {
                return;
            }
            ctx.request_repaint();
        }

        let client = match TvhClient::with_timeout(&url, &user, &password, PAGE_TIMEOUT) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Err(e.to_string()));
                ctx.request_repaint();
                return;
            }
        };

        let mut all: Vec<EpgEvent> = Vec::new();
        let mut offset = 0usize;
        loop {
            match client.epg_events_page(offset, PAGE_SIZE) {
                Ok((page, total)) => {
                    let got = page.len();
                    all.extend(page);
                    let done = got < PAGE_SIZE || all.len() >= total;
                    let progress = EpgProgress {
                        events: all.clone(),
                        loaded: all.len(),
                        total: if done { all.len() } else { total },
                    };
                    if tx.send(Ok(progress)).is_err() {
                        return;
                    }
                    ctx.request_repaint();
                    if done {
                        break;
                    }
                    offset += got;
                }
                Err(e) => {
                    let _ = tx.send(Err(e.to_string()));
                    ctx.request_repaint();
                    return;
                }
            }
        }

        save_cache(&server_id, &all);
    });
}
