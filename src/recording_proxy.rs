//! A tiny local HTTP/1.1 server whose only job is to let mpv seek
//! natively (via ordinary HTTP Range requests) through a TVHeadend
//! recording, instead of us reimplementing that ourselves.
//!
//! Two earlier approaches were tried here and abandoned:
//!
//! - Pointing mpv straight at TVHeadend's `dvrfile/<uuid>` URL: just
//!   hung for ~20s with nothing on screen - no error, no
//!   `paused-for-cache` signal either. Likely some connection/protocol
//!   detail between mpv's own HTTP client and TVHeadend specifically,
//!   since the exact same URL downloads fine through our own `reqwest`-
//!   based code (`recordings::spawn_download`).
//! - Downloading to a local file and handing mpv *that*, with manual
//!   byte-offset bookkeeping to support seeking ahead of what had been
//!   downloaded so far: works fine for straight-through playback, but
//!   seeking into a not-yet-downloaded part reproduces a real,
//!   long-standing mpv limitation - see mpv issue #6465, "mpv hangs when
//!   seeking on growing file" (15-20s hangs, matching exactly what David
//!   saw: the position snapping back and playback stalling after
//!   clicking the seek bar).
//!
//! This sidesteps both problems at once: mpv is pointed at
//! `http://127.0.0.1:<port>/` instead of either of those, which looks to
//! mpv exactly like any ordinary, complete, Range-capable network video -
//! the case ffmpeg's own HTTP demuxer is designed and tested for, not a
//! still-growing local file. Every request that arrives here is
//! forwarded upstream to TVHeadend with the *same* `Range` header mpv
//! sent, using the same `reqwest` client this app's downloads already
//! use reliably, and the response (status, `Content-Length`/
//! `Content-Range`/`Content-Type`, body) is streamed straight back
//! unmodified. No local caching, no byte-offset bookkeeping - just a
//! transparent relay.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A running proxy - drop it to stop it (see `Drop` impl).
pub struct RecordingProxy {
    port: u16,
    shutdown: Arc<AtomicBool>,
}

impl RecordingProxy {
    /// What to hand `MpvPlayer::load`.
    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}/", self.port)
    }
}

impl Drop for RecordingProxy {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // The listener thread is blocked in `TcpListener::accept()` -
        // wake it up with a throwaway connection so it notices
        // `shutdown` and exits, instead of leaking a thread parked
        // forever waiting for the next connection that will now never
        // come (the accepting handler just sees an empty/malformed
        // request from this and does nothing, see `handle_connection`).
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

/// Starts the proxy on an OS-assigned local port, relaying every request
/// to `upstream_url` (a ready-to-fetch, ticketed TVHeadend recording URL
/// - see `TvhClient::dvr_urls`).
pub fn spawn(upstream_url: String) -> Result<RecordingProxy, String> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_thread = shutdown.clone();

    std::thread::spawn(move || {
        for conn in listener.incoming() {
            if shutdown_thread.load(Ordering::Relaxed) {
                break;
            }
            let Ok(stream) = conn else { continue };
            let url = upstream_url.clone();
            std::thread::spawn(move || handle_connection(stream, &url));
        }
    });

    Ok(RecordingProxy { port, shutdown })
}

/// Handles exactly one connection from mpv: reads its request just far
/// enough to pull out a `Range` header (if any), forwards that to
/// `upstream_url`, and relays the response back verbatim. Method/path
/// are ignored entirely - there's only ever one resource here.
fn handle_connection(mut stream: TcpStream, upstream_url: &str) {
    let Ok(range) = read_range_header(&mut stream) else {
        return;
    };

    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(3600))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            write_status(&mut stream, 502);
            return;
        }
    };
    let mut request = client.get(upstream_url);
    if let Some(range) = &range {
        request = request.header(reqwest::header::RANGE, range.clone());
    }
    let mut response = match request.send() {
        Ok(r) => r,
        Err(_) => {
            write_status(&mut stream, 502);
            return;
        }
    };
    if !response.status().is_success() {
        write_status(&mut stream, response.status().as_u16());
        return;
    }

    let mut head = format!(
        "HTTP/1.1 {} {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n",
        response.status().as_u16(),
        response.status().canonical_reason().unwrap_or(""),
    );
    for name in [
        reqwest::header::CONTENT_LENGTH,
        reqwest::header::CONTENT_RANGE,
        reqwest::header::CONTENT_TYPE,
    ] {
        if let Some(value) = response.headers().get(&name) {
            if let Ok(value) = value.to_str() {
                head.push_str(&format!("{name}: {value}\r\n"));
            }
        }
    }
    head.push_str("\r\n");
    if stream.write_all(head.as_bytes()).is_err() {
        return;
    }

    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = match response.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if stream.write_all(&buf[..n]).is_err() {
            break;
        }
    }
}

/// Reads just enough of an incoming HTTP request to find its headers
/// (up to the blank line terminating them) and pulls out `Range:` if
/// present. Doesn't care about the request line or any other header -
/// this proxy only ever serves one resource.
fn read_range_header(stream: &mut TcpStream) -> std::io::Result<Option<String>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        // Defensive - a real request from mpv is a few hundred bytes at
        // most; anything wildly larger than this is not one.
        if buf.len() > 16 * 1024 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    for line in text.lines() {
        if let Some(value) = line
            .strip_prefix("Range:")
            .or_else(|| line.strip_prefix("range:"))
        {
            return Ok(Some(value.trim().to_string()));
        }
    }
    Ok(None)
}

fn write_status(stream: &mut TcpStream, code: u16) {
    let _ = stream.write_all(format!("HTTP/1.1 {code} Error\r\nConnection: close\r\n\r\n").as_bytes());
}
