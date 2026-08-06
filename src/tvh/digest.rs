//! HTTP authentication for TVHeadend requests.
//!
//! This is a direct port of the logic in the webOS app's
//! `service/httpproxyhandler.js` (functions `digestAuth` / `basicAuth` /
//! `handleAuthentication`), which we already validated against a real
//! TVHeadend server. Supports:
//!   - Digest auth with MD5 (default/legacy), SHA-256 and SHA-512-256
//!     (TVHeadend >= 4.3 can ask for any of these via the `algorithm`
//!     challenge parameter).
//!   - Basic auth, as a fallback if the server asks for it.
//!
//! TVHeadend digest quirk (see original JS comment): it wants the
//! `algorithm` value quoted in the response header even though RFC 7616
//! says it shouldn't be. We match that behavior here.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use md5::Md5;
use sha2::{Digest as ShaDigestTrait, Sha256, Sha512_256};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

/// A parsed `WWW-Authenticate` challenge header, e.g.:
/// `Digest realm="tvheadend", qop="auth", nonce="...", algorithm=SHA-256`
#[derive(Debug, Clone)]
pub struct Challenge {
    pub scheme: String,
    params: HashMap<String, String>,
}

impl Challenge {
    /// Parse a `WWW-Authenticate` header value. Returns `None` if it
    /// doesn't look like a challenge at all.
    pub fn parse(header: &str) -> Option<Self> {
        let header = header.trim();
        let mut split = header.splitn(2, char::is_whitespace);
        let scheme = split.next()?.trim().to_string();
        let rest = split.next().unwrap_or("").trim();

        // Split on commas that are outside of quoted strings.
        let mut params = HashMap::new();
        let mut field = String::new();
        let mut in_quotes = false;
        let mut fields: Vec<String> = Vec::new();
        for c in rest.chars() {
            match c {
                '"' => {
                    in_quotes = !in_quotes;
                    field.push(c);
                }
                ',' if !in_quotes => {
                    fields.push(std::mem::take(&mut field));
                }
                _ => field.push(c),
            }
        }
        if !field.trim().is_empty() {
            fields.push(field);
        }

        for raw in fields {
            let raw = raw.trim();
            if let Some((key, value)) = raw.split_once('=') {
                let value = value.trim().trim_matches('"').to_string();
                params.insert(key.trim().to_ascii_lowercase(), value);
            }
        }

        Some(Challenge { scheme, params })
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }

    pub fn is_digest(&self) -> bool {
        self.scheme.eq_ignore_ascii_case("digest")
    }

    pub fn is_basic(&self) -> bool {
        self.scheme.eq_ignore_ascii_case("basic")
    }
}

/// Build the `Authorization` header value to send in response to `challenge`
/// for a request `method` (e.g. "GET") against `path` (e.g.
/// "/api/serverinfo", must match exactly what's sent on the wire).
pub fn authorization_header(
    challenge: &Challenge,
    user: &str,
    password: &str,
    method: &str,
    path: &str,
) -> Option<String> {
    if challenge.is_basic() {
        Some(basic_auth(user, password))
    } else if challenge.is_digest() {
        digest_auth(challenge, user, password, method, path)
    } else {
        None
    }
}

fn basic_auth(user: &str, password: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:{password}")))
}

fn digest_auth(
    challenge: &Challenge,
    user: &str,
    password: &str,
    method: &str,
    path: &str,
) -> Option<String> {
    let realm = challenge.get("realm").unwrap_or("");
    let nonce = challenge.get("nonce")?;
    let qop = challenge.get("qop").unwrap_or("auth");
    let opaque = challenge.get("opaque");

    // TVHeadend sends e.g. algorithm="SHA-256" or algorithm=SHA-256 (unquoted,
    // both are handled by the parser above). Default to MD5 when absent.
    let (algorithm_label, hash): (&str, Box<dyn Fn(&str) -> String>) =
        match challenge.get("algorithm") {
            Some(a) if a.eq_ignore_ascii_case("SHA-256") => {
                ("SHA-256", Box::new(|s: &str| hex(&Sha256::digest(s.as_bytes()))))
            }
            Some(a) if a.eq_ignore_ascii_case("SHA-512-256") => (
                "SHA-512-256",
                Box::new(|s: &str| hex(&Sha512_256::digest(s.as_bytes()))),
            ),
            _ => ("MD5", Box::new(|s: &str| hex(&Md5::digest(s.as_bytes())))),
        };

    let cnonce = client_nonce();
    let nc = "00000001";

    // HA1 = hash(user:realm:password)
    let ha1 = hash(&format!("{user}:{realm}:{password}"));
    // HA2 = hash(method:path)
    let ha2 = hash(&format!("{method}:{path}"));
    // response = hash(HA1:nonce:nc:cnonce:qop:HA2)
    let response = hash(&format!("{ha1}:{nonce}:{nc}:{cnonce}:{qop}:{ha2}"));

    let mut header = format!(
        "Digest username=\"{user}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{path}\", \
         algorithm=\"{algorithm_label}\", cnonce=\"{cnonce}\", nc={nc}, qop={qop}, response=\"{response}\""
    );
    if let Some(opaque) = opaque {
        let _ = write!(header, ", opaque=\"{opaque}\"");
    }

    Some(header)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Generate a client nonce. We don't depend on the `rand` crate to keep the
/// dependency tree small; this mixes the current time with a per-process
/// atomic counter and hashes it, which is more than good enough entropy for
/// a digest-auth cnonce (it only needs to be unique-ish per request, not
/// cryptographically secure).
fn client_nonce() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seed = format!("{now}-{n}-{:?}", std::thread::current().id());
    hex(&Sha256::digest(seed.as_bytes()))[..20].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_challenge() {
        let header = r#"Digest realm="tvheadend", qop="auth", nonce="abc123", algorithm="SHA-256", opaque="xyz""#;
        let c = Challenge::parse(header).unwrap();
        assert!(c.is_digest());
        assert_eq!(c.get("realm"), Some("tvheadend"));
        assert_eq!(c.get("nonce"), Some("abc123"));
        assert_eq!(c.get("algorithm"), Some("SHA-256"));
        assert_eq!(c.get("opaque"), Some("xyz"));
    }

    #[test]
    fn md5_hash_has_expected_length() {
        // HA1 = MD5("user:realm:pass") should be a 32-char hex string.
        let ha1 = hex(&Md5::digest(b"user:realm:pass"));
        assert_eq!(ha1.len(), 32);
    }
}
