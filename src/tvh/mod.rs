//! TVHeadend REST/M3U client.
//!
//! Ported from the webOS app's `src/services/TVHDataService.ts` (channel
//! list via M3U playlist, `api/serverinfo`) combined with the digest-auth
//! logic from `service/httpproxyhandler.js`. This dry-run version only
//! covers what the connect screen + channel list need; DVR/EPG endpoints
//! from the original app are not ported yet.

pub mod digest;
pub mod m3u;

use serde::Deserialize;
use std::fmt;
use std::time::Duration;

/// A single playable channel, as shown in the channel list.
#[derive(Debug, Clone)]
pub struct Channel {
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
    /// with or without a trailing slash, e.g. `192.168.0.10:9981`.
    pub fn new(base_url: &str, user: &str, password: &str) -> Result<Self, TvhError> {
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
            .timeout(Duration::from_secs(10))
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
    /// number.
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
                number: item.channel_number,
                name: item.channel_name,
                logo_url: item.logo_url,
                stream_url: item.stream_url,
            })
            .collect();

        channels.sort_by(|a, b| {
            let na: f64 = a.number.parse().unwrap_or(f64::MAX);
            let nb: f64 = b.number.parse().unwrap_or(f64::MAX);
            na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal)
        });

        Ok(channels)
    }
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
