//! TVHeadend REST/M3U client.
//!
//! Ported from the webOS app's `src/services/TVHDataService.ts` (channel
//! list via M3U playlist, `api/serverinfo`) combined with the digest-auth
//! logic from `service/httpproxyhandler.js`. Also covers channel tags
//! (`api/channeltag/grid`, `api/channel/grid`), the EPG grid
//! (`api/epg/events/grid`, see `EpgEvent`/`epg_events_page` + `src/epg.rs`), and
//! DVR/recordings (`api/dvr/entry/*`, `api/dvr/autorec/*` - see `DvrEntry`
//! and the `dvr_*`/`autorec_*` methods below, plus `src/recordings.rs` for
//! the app-side background fetch/download orchestration).

pub mod digest;
pub mod m3u;

use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// A single playable channel, as shown in the channel list.
#[derive(Debug, Clone)]
pub struct Channel {
    /// TVHeadend's own channel uuid (from the M3U playlist's `tvg-id`
    /// attribute, which TVHeadend sets to exactly this). Used to join
    /// against `api/channel/grid` (tag membership) and `EpgEvent::channel_uuid`
    /// (EPG) - not shown in the UI itself.
    pub channel_id: String,
    pub number: String,
    pub name: String,
    pub logo_url: String,
    pub stream_url: String,
}

/// Response of `GET api/serverinfo`. Fields are optional since we only rely
/// on them for a friendly "connected to ..." message - a missing field
/// should never break the connection.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerInfo {
    pub sw_version: Option<String>,
    pub api_version: Option<i64>,
    pub name: Option<String>,
}

/// A channel tag ("group"), as configured on the server. Used to let the
/// user pick a subset of channels to show.
///
/// Fetched via `api/channeltag/grid`, following TVHeadend's general
/// `api/<class>/grid` admin-API convention. Confirmed working against a
/// real server (the Test button in the server form does show real tags).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ChannelTag {
    pub uuid: String,
    pub name: String,
}

/// Just enough of `api/channel/grid`'s per-channel object to know which
/// tags a channel carries - used only to build the uuid->tags lookup for
/// [`TvhClient::channels_for_tags`], never exposed outside this module.
#[derive(Debug, Clone, Default, Deserialize)]
struct AdminChannel {
    uuid: String,
    #[serde(default)]
    tags: Vec<String>,
}

/// Just enough of one `api/dvr/config/grid` entry to resolve a recording
/// profile's uuid - see `TvhClient::dvr_default_config_uuid`.
#[derive(Debug, Clone, Default, Deserialize)]
struct DvrConfig {
    uuid: String,
    #[serde(default)]
    name: String,
}

/// One EPG (programme guide) entry, as returned by `api/epg/events/grid`.
/// TVHeadend's own docs say EPG sources vary in what they provide and
/// "any items which have no data are omitted" - so everything is
/// `#[serde(default)]` here and [`TvhClient::epg_events_page`] filters out any
/// entry missing the fields actually needed to place it on a timeline
/// (`channel_uuid`, and `stop` strictly after `start`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EpgEvent {
    #[serde(rename = "eventId", default)]
    pub event_id: i64,
    #[serde(rename = "channelUuid", default)]
    pub channel_uuid: String,
    /// Unix timestamp (seconds, UTC).
    #[serde(default)]
    pub start: i64,
    /// Unix timestamp (seconds, UTC).
    #[serde(default)]
    pub stop: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

/// One DVR entry (a timer, running recording, or finished/failed
/// recording - `sched_status` tells them apart), as returned by
/// `api/dvr/entry/grid_upcoming`/`grid_finished`/`grid_failed`. Uses the
/// `disp_*` fields TVHeadend provides for display (plain strings) instead
/// of `title`/`summary`/`description` (which are objects keyed by
/// language code, more hassle than they're worth here).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DvrEntry {
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub disp_title: String,
    #[serde(default)]
    pub disp_subtitle: String,
    /// TVHeadend's own "best available blurb" field - picks between
    /// subtitle/summary/description depending on what the broadcaster
    /// actually sent, so we don't have to.
    #[serde(default)]
    pub disp_extratext: String,
    #[serde(default)]
    pub channelname: String,
    /// Unix timestamps (seconds, UTC).
    #[serde(default)]
    pub start: i64,
    #[serde(default)]
    pub stop: i64,
    /// Human-readable one-liner, e.g. "Completed OK", "Scheduled for
    /// recording", "Too many data errors".
    #[serde(default)]
    pub status: String,
    /// Machine-readable status: "scheduled", "recording", "completed", ...
    #[serde(default)]
    pub sched_status: String,
    /// Bytes; 0 for entries that haven't finished (yet).
    #[serde(default)]
    pub filesize: i64,
    /// Relative path, e.g. `"dvrfile/<uuid>"` - only present once a
    /// recording exists on disk. Not directly fetchable without auth; see
    /// [`TvhClient::dvr_urls`] for a ready-to-use, ticketed version.
    #[serde(default)]
    pub url: String,
    /// Full path on the *server*, e.g. `/video/tvheadend/Foo-4.ts` - only
    /// used client-side to sniff the file extension for downloads (see
    /// `recordings::safe_filename`), never as a local path.
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub errors: i64,
    #[serde(default)]
    pub errorcode: i64,
}

// Note: `#[serde(default)]` on `entries` below makes serde_derive add a
// (structurally unnecessary, but that's how its bound-inference works)
// `T: Default` bound to `GridResponse<T>`'s generated `Deserialize` impl -
// so every `T` used with `GridResponse<T>` needs `#[derive(Default)]`
// too (see `ChannelTag` above).
#[derive(Debug, Deserialize)]
struct GridResponse<T> {
    #[serde(default)]
    entries: Vec<T>,
    /// Total number of rows matching the query on the server, regardless
    /// of `limit`/`start` paging - lets [`TvhClient::epg_events_page`]
    /// callers know when they've fetched everything without a trailing
    /// "empty page" request.
    #[serde(rename = "totalCount", default)]
    total_count: usize,
}

#[derive(Debug)]
pub enum TvhError {
    InvalidUrl(String),
    Request(String),
    Http { status: u16, body: String },
    AuthFailed,
    Parse(String),
}

impl fmt::Display for TvhError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TvhError::InvalidUrl(s) => write!(f, "Neplatná adresa serveru: {s}"),
            TvhError::Request(s) => write!(f, "Chyba spojení: {s}"),
            TvhError::Http { status, body } => {
                let snippet: String = body.chars().take(200).collect();
                write!(f, "Server odpověděl chybou {status}: {snippet}")
            }
            TvhError::AuthFailed => write!(f, "Přihlášení se nezdařilo (špatné jméno/heslo?)"),
            TvhError::Parse(s) => write!(f, "Nepodařilo se zpracovat odpověď serveru: {s}"),
        }
    }
}

impl std::error::Error for TvhError {}

pub struct TvhClient {
    base_url: String,
    user: String,
    password: String,
    http: reqwest::blocking::Client,
}

impl TvhClient {
    /// `base_url` may be given without scheme (defaults to `http://`) and
    /// with or without a trailing slash, e.g. `192.168.0.10:9981`. 10s
    /// request timeout - fine for the lightweight calls (login,
    /// serverinfo, channel/tag lists). For anything that might take
    /// longer on a big install (the EPG grid), use
    /// [`with_timeout`](Self::with_timeout) instead.
    pub fn new(base_url: &str, user: &str, password: &str) -> Result<Self, TvhError> {
        Self::with_timeout(base_url, user, password, Duration::from_secs(10))
    }

    /// Same as [`new`](Self::new), but with a caller-chosen request
    /// timeout instead of the default 10s.
    pub fn with_timeout(
        base_url: &str,
        user: &str,
        password: &str,
        timeout: Duration,
    ) -> Result<Self, TvhError> {
        let base_url = base_url.trim();
        if base_url.is_empty() {
            return Err(TvhError::InvalidUrl("adresa je prázdná".into()));
        }
        let mut base_url = base_url.to_string();
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            base_url = format!("http://{base_url}");
        }
        if !base_url.ends_with('/') {
            base_url.push('/');
        }
        // Validate early so connection errors are reported up front.
        reqwest::Url::parse(&base_url).map_err(|e| TvhError::InvalidUrl(e.to_string()))?;

        let http = reqwest::blocking::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| TvhError::Request(e.to_string()))?;

        Ok(Self {
            base_url,
            user: user.to_string(),
            password: password.to_string(),
            http,
        })
    }

    /// GET `path` (relative to the base URL), transparently handling a
    /// Digest or Basic auth challenge on the first 401 response.
    fn get(&self, path: &str) -> Result<String, TvhError> {
        let url = format!("{}{}", self.base_url, path);

        let resp = self
            .http
            .get(&url)
            .send()
            .map_err(|e| TvhError::Request(e.to_string()))?;

        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            return finish(resp);
        }

        let header = resp
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .ok_or(TvhError::AuthFailed)?
            .to_string();

        let challenge = digest::Challenge::parse(&header).ok_or(TvhError::AuthFailed)?;
        let request_target = request_target(&url);
        let auth_header =
            digest::authorization_header(&challenge, &self.user, &self.password, "GET", &request_target)
                .ok_or(TvhError::AuthFailed)?;

        let resp2 = self
            .http
            .get(&url)
            .header(reqwest::header::AUTHORIZATION, auth_header)
            .send()
            .map_err(|e| TvhError::Request(e.to_string()))?;

        finish(resp2)
    }

    /// `GET api/serverinfo` - used on the connect screen to confirm we can
    /// actually reach and authenticate against the server.
    pub fn server_info(&self) -> Result<ServerInfo, TvhError> {
        let body = self.get("api/serverinfo")?;
        serde_json::from_str(&body).map_err(|e| TvhError::Parse(e.to_string()))
    }

    /// Channel list via the M3U playlist endpoint (same source the webOS
    /// app and any regular IPTV player would use), sorted by channel
    /// number. Each channel's `channel_id` comes from the playlist's
    /// `tvg-id` attribute (TVHeadend sets this to the channel's own
    /// uuid) - used to cross-reference tags and EPG data.
    pub fn channels(&self) -> Result<Vec<Channel>, TvhError> {
        let with_auth = !self.user.is_empty() || !self.password.is_empty();
        let primary = if with_auth {
            "playlist/auth/channels"
        } else {
            "playlist/channels"
        };

        let body = match self.get(primary) {
            Ok(body) => body,
            // Older TVHeadend (< 4.3) doesn't have the `auth/` path -
            // fall back to the plain one.
            Err(_) if with_auth => self.get("playlist/channels")?,
            Err(e) => return Err(e),
        };

        let parsed = m3u::parse(&body).map_err(TvhError::Parse)?;

        let mut channels: Vec<Channel> = parsed
            .items
            .into_iter()
            .filter(|item| !item.channel_number.is_empty())
            .map(|item| Channel {
                channel_id: item.channel_id,
                number: item.channel_number,
                name: item.channel_name,
                logo_url: item.logo_url,
                stream_url: item.stream_url,
            })
            .collect();

        channels.sort_by(channel_number_order);

        Ok(channels)
    }

    /// List of channel tags configured on the server. Best-effort: if the
    /// endpoint doesn't exist / doesn't parse as expected, callers should
    /// treat that as "no tags available" rather than a hard failure (see
    /// `ChannelTag` docs).
    pub fn channel_tags(&self) -> Result<Vec<ChannelTag>, TvhError> {
        let body = self.get("api/channeltag/grid")?;
        let parsed: GridResponse<ChannelTag> =
            serde_json::from_str(&body).map_err(|e| TvhError::Parse(e.to_string()))?;
        Ok(parsed.entries)
    }

    /// Channel list restricted to the given channel tags (by uuid).
    /// Empty `tag_uuids` behaves exactly like [`channels`](Self::channels)
    /// (no filtering).
    ///
    /// Earlier version of this tried the playlist endpoint's own
    /// `?tag=<uuid>` query parameter, which turned out to not actually
    /// filter anything on a real server. This version instead: fetches
    /// the normal M3U playlist (unfiltered, same as `channels()`), fetches
    /// `api/channel/grid` to learn each channel's `tags` (a uuid array),
    /// and keeps only the M3U entries whose `tvg-id` (which TVHeadend sets
    /// to the channel's own uuid) is a channel that carries at least one
    /// of the selected tags.
    pub fn channels_for_tags(&self, tag_uuids: &[String]) -> Result<Vec<Channel>, TvhError> {
        if tag_uuids.is_empty() {
            return self.channels();
        }

        let all = self.channels()?;

        let body = self.get("api/channel/grid?limit=100000")?;
        let parsed: GridResponse<AdminChannel> =
            serde_json::from_str(&body).map_err(|e| TvhError::Parse(e.to_string()))?;

        let wanted: std::collections::HashSet<&str> =
            tag_uuids.iter().map(String::as_str).collect();
        let allowed_channel_uuids: std::collections::HashSet<String> = parsed
            .entries
            .into_iter()
            .filter(|c| c.tags.iter().any(|t| wanted.contains(t.as_str())))
            .map(|c| c.uuid)
            .collect();

        Ok(all
            .into_iter()
            .filter(|channel| {
                !channel.channel_id.is_empty() && allowed_channel_uuids.contains(&channel.channel_id)
            })
            .collect())
    }

    /// One page of the EPG grid (`start`/`limit` = row offset/count,
    /// sorted by event start time - the server default). Returns the
    /// page's events plus the *total* row count matching the query
    /// (`totalCount`, independent of `limit`), so callers doing their own
    /// pagination loop (see `src/epg.rs`'s progressive background sync,
    /// the only caller - TVHeadend's own docs show a `totalCount` of
    /// 18575 in their example, so a one-shot "fetch it all" helper isn't
    /// something this app wants) know when they're done without a
    /// trailing empty-page request.
    ///
    /// Sorting by start means the *earliest* pages are the
    /// soonest-to-air events - i.e. exactly "what's on now/soon" for
    /// every channel - since TVHeadend prunes fully-elapsed EPG entries,
    /// so fetching just the first page or two is already useful on its
    /// own well before the rest of the guide has downloaded.
    pub fn epg_events_page(&self, start: usize, limit: usize) -> Result<(Vec<EpgEvent>, usize), TvhError> {
        let body = self.get(&format!("api/epg/events/grid?start={start}&limit={limit}&sort=start"))?;
        let parsed: GridResponse<EpgEvent> =
            serde_json::from_str(&body).map_err(|e| TvhError::Parse(e.to_string()))?;
        let events = parsed
            .entries
            .into_iter()
            .filter(|e| !e.channel_uuid.is_empty() && e.stop > e.start)
            .collect();
        Ok((events, parsed.total_count))
    }

    // ---- DVR: timers/recordings ---------------------------------------

    /// Currently-scheduled recordings (timers not yet started).
    pub fn dvr_upcoming(&self) -> Result<Vec<DvrEntry>, TvhError> {
        self.dvr_grid("api/dvr/entry/grid_upcoming")
    }

    /// Recordings that completed successfully and are still on disk.
    pub fn dvr_finished(&self) -> Result<Vec<DvrEntry>, TvhError> {
        self.dvr_grid("api/dvr/entry/grid_finished")
    }

    /// Recordings that failed (data errors, aborted, ...) - `status`
    /// explains why. Still generally playable/downloadable like a
    /// finished one, just flagged.
    pub fn dvr_failed(&self) -> Result<Vec<DvrEntry>, TvhError> {
        self.dvr_grid("api/dvr/entry/grid_failed")
    }

    fn dvr_grid(&self, path: &str) -> Result<Vec<DvrEntry>, TvhError> {
        let body = self.get(&format!("{path}?limit=100000"))?;
        let parsed: GridResponse<DvrEntry> =
            serde_json::from_str(&body).map_err(|e| TvhError::Parse(e.to_string()))?;
        Ok(parsed.entries)
    }

    /// Cancels a pending timer, or aborts a currently-running recording
    /// (which then shows up under "failed" - the file already written is
    /// kept, not deleted).
    pub fn dvr_cancel(&self, uuid: &str) -> Result<(), TvhError> {
        self.post("api/dvr/entry/cancel", &[("uuid", uuid)])
    }

    /// Deletes a finished (or failed) recording's file from storage and
    /// removes its log entry.
    pub fn dvr_remove(&self, uuid: &str) -> Result<(), TvhError> {
        self.post("api/dvr/entry/remove", &[("uuid", uuid)])
    }

    /// Ready-to-use, pre-authenticated URLs for every recording's file
    /// (`dvrfile/<uuid>`), keyed by that recording's uuid - suitable to
    /// hand straight to mpv or a plain HTTP GET (no separate auth
    /// headers/digest challenge needed), exactly like `channels()`'s
    /// `stream_url` already does for live channels: this fetches TVHeadend's
    /// own `playlist/auth/recordings` M3U (falling back to the plain
    /// `playlist/recordings` path on older servers without it, same as
    /// `channels()`) and pulls the uuid straight out of each entry's
    /// `dvrfile/<uuid>...` URL.
    pub fn dvr_urls(&self) -> Result<std::collections::HashMap<String, String>, TvhError> {
        let with_auth = !self.user.is_empty() || !self.password.is_empty();
        let primary = if with_auth {
            "playlist/auth/recordings"
        } else {
            "playlist/recordings"
        };
        let body = match self.get(primary) {
            Ok(body) => body,
            Err(_) if with_auth => self.get("playlist/recordings")?,
            Err(e) => return Err(e),
        };

        let mut map = std::collections::HashMap::new();
        for line in body.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(uuid) = dvrfile_uuid(line) {
                map.insert(uuid, line.to_string());
            }
        }
        Ok(map)
    }

    // ---- DVR: creating recordings from the EPG --------------------------

    /// Resolves the DVR recording profile ("config") to use for a new
    /// timer/autorec rule created from the EPG tab's right-click menu -
    /// the default profile (its `name` is blank, see `dvr/config/grid`'s
    /// docs) if there is one, otherwise whatever profile happens to be
    /// listed first. Always fetched fresh rather than cached: profiles
    /// are rarely changed, but this keeps `dvr_record_event`/
    /// `dvr_autorec_create_for_title` simple and correct even if they are.
    pub fn dvr_default_config_uuid(&self) -> Result<String, TvhError> {
        let body = self.get("api/dvr/config/grid?limit=100000")?;
        let parsed: GridResponse<DvrConfig> =
            serde_json::from_str(&body).map_err(|e| TvhError::Parse(e.to_string()))?;
        parsed
            .entries
            .iter()
            .find(|c| c.name.is_empty())
            .or_else(|| parsed.entries.first())
            .map(|c| c.uuid.clone())
            .ok_or_else(|| {
                TvhError::Parse("Server nemá nastavený žádný profil nahrávání.".to_string())
            })
    }

    /// Schedules a one-time recording of a single EPG event ("Nahrát" in
    /// the EPG tab's right-click menu).
    pub fn dvr_record_event(&self, event_id: i64, config_uuid: &str) -> Result<(), TvhError> {
        self.post(
            "api/dvr/entry/create_by_event",
            &[
                ("event_id", event_id.to_string().as_str()),
                ("config_uuid", config_uuid),
            ],
        )
    }

    /// Creates a recurring recording ("Nahrávat opakovaně" in the EPG
    /// tab's right-click menu) matching every future EPG event with this
    /// exact title on this channel. Title+channel matching (via
    /// `dvr/autorec/create`'s search-parameter `conf`), rather than
    /// `dvr/autorec/create_by_series`'s CRID-based matching, since CRID/
    /// series-link metadata isn't guaranteed present in every EPG source
    /// while `title` always is (see `EpgEvent`).
    pub fn dvr_autorec_create_for_title(
        &self,
        title: &str,
        channel_uuid: &str,
        config_uuid: &str,
    ) -> Result<(), TvhError> {
        let conf = serde_json::json!({
            "enabled": true,
            "title": title,
            "fulltext": false,
            "channel": channel_uuid,
        });
        let conf_str =
            serde_json::to_string(&conf).map_err(|e| TvhError::Parse(e.to_string()))?;
        self.post(
            "api/dvr/autorec/create",
            &[("conf", conf_str.as_str()), ("config_uuid", config_uuid)],
        )
    }

    // ---- DVR: autorec (series/search timers) ---------------------------

    /// Autorec ("opakující se nahrávky") entries as raw JSON objects
    /// rather than a typed struct - deliberately, since editing one means
    /// sending the *whole* object back via `idnode/save` (see
    /// `autorec_save`'s doc comment for why), and a typed struct covering
    /// every field TVHeadend might have set would be brittle for no
    /// benefit here.
    pub fn autorec_list(&self) -> Result<Vec<serde_json::Value>, TvhError> {
        let body = self.get("api/dvr/autorec/grid?limit=100000")?;
        let parsed: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| TvhError::Parse(e.to_string()))?;
        Ok(parsed
            .get("entries")
            .and_then(|e| e.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// Deletes an autorec rule (and, per TVHeadend's own UI behaviour,
    /// every pending timer it had already scheduled).
    pub fn autorec_delete(&self, uuid: &str) -> Result<(), TvhError> {
        self.post("api/idnode/delete", &[("uuid", uuid)])
    }

    /// Saves an edited autorec entry. `node` must be the *complete*
    /// object as returned by [`autorec_list`](Self::autorec_list) (it
    /// already contains `uuid`), with only the fields the user actually
    /// changed modified in place - `idnode/save` *replaces* the whole
    /// idnode with what's sent, it does not merge, so sending a partial
    /// object would silently reset every field we don't expose in our
    /// edit form back to its type default.
    pub fn autorec_save(&self, node: &serde_json::Value) -> Result<(), TvhError> {
        let node_str = serde_json::to_string(node).map_err(|e| TvhError::Parse(e.to_string()))?;
        self.post("api/idnode/save", &[("node", node_str.as_str())])
    }

    /// POST `path` with a form body, same Digest/Basic auth-retry dance as
    /// [`get`](Self::get). Used for the write-y DVR/autorec actions
    /// (cancel/remove/delete/save) - their response bodies are small
    /// status objects we don't need, so this just returns `()` on success.
    fn post(&self, path: &str, form: &[(&str, &str)]) -> Result<(), TvhError> {
        let url = format!("{}{}", self.base_url, path);

        let resp = self
            .http
            .post(&url)
            .form(form)
            .send()
            .map_err(|e| TvhError::Request(e.to_string()))?;

        if resp.status() != reqwest::StatusCode::UNAUTHORIZED {
            finish(resp)?;
            return Ok(());
        }

        let header = resp
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .ok_or(TvhError::AuthFailed)?
            .to_string();

        let challenge = digest::Challenge::parse(&header).ok_or(TvhError::AuthFailed)?;
        let request_target = request_target(&url);
        let auth_header = digest::authorization_header(
            &challenge,
            &self.user,
            &self.password,
            "POST",
            &request_target,
        )
        .ok_or(TvhError::AuthFailed)?;

        let resp2 = self
            .http
            .post(&url)
            .header(reqwest::header::AUTHORIZATION, auth_header)
            .form(form)
            .send()
            .map_err(|e| TvhError::Request(e.to_string()))?;

        finish(resp2)?;
        Ok(())
    }
}

/// Pulls the uuid out of a `.../dvrfile/<uuid>...` URL (path or query
/// string may follow) - see [`TvhClient::dvr_urls`].
fn dvrfile_uuid(url: &str) -> Option<String> {
    let (_, after) = url.split_once("dvrfile/")?;
    let uuid: String = after
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    if uuid.is_empty() {
        None
    } else {
        Some(uuid)
    }
}

fn channel_number_order(a: &Channel, b: &Channel) -> std::cmp::Ordering {
    let na: f64 = a.number.parse().unwrap_or(f64::MAX);
    let nb: f64 = b.number.parse().unwrap_or(f64::MAX);
    na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal)
}

fn finish(resp: reqwest::blocking::Response) -> Result<String, TvhError> {
    let status = resp.status();
    if status.is_success() {
        resp.text().map_err(|e| TvhError::Request(e.to_string()))
    } else {
        let status_code = status.as_u16();
        let body = resp.text().unwrap_or_default();
        Err(TvhError::Http {
            status: status_code,
            body,
        })
    }
}

/// The Digest HA2 hash must use the exact `request-target` sent on the
/// wire (path + query, no scheme/host).
fn request_target(full_url: &str) -> String {
    match reqwest::Url::parse(full_url) {
        Ok(u) => {
            let mut target = u.path().to_string();
            if let Some(q) = u.query() {
                target.push('?');
                target.push_str(q);
            }
            target
        }
        Err(_) => full_url.to_string(),
    }
}
