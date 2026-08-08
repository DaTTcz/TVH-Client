//! Minimal M3U(+EXTINF attributes) parser, ported from the webOS app's
//! `src/utils/M3UParser.ts`. TVHeadend's `playlist/channels` (or
//! `playlist/auth/channels`) endpoint returns entries like:
//!
//! ```text
//! #EXTM3U x-tvg-url="http://host:9981/xmltv/channels"
//! #EXTINF:-1 logo="http://host:9981/imagecache/7167?auth=..." tvg-id="978ff..." tvg-chno="1",Das Erste HD
//! http://host:9981/stream/channelid/1241288599?auth=...&profile=pass
//! ```

#[derive(Debug, Clone)]
pub struct M3uItem {
    pub channel_id: String,
    pub channel_number: String,
    pub channel_name: String,
    pub logo_url: String,
    pub stream_url: String,
}

#[derive(Debug, Clone, Default)]
pub struct ParseResult {
    pub items: Vec<M3uItem>,
}

/// Parse raw M3U playlist text into channel entries.
pub fn parse(content: &str) -> Result<ParseResult, String> {
    // Split into blocks, one per #EXTINF entry (mirrors the JS
    // `split(/(?=#EXTINF)/)` lookahead split).
    let mut blocks = Vec::new();
    let mut current = String::new();
    for line in content.lines() {
        if line.starts_with("#EXTINF") && !current.is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push('\n');
        }
        current.push_str(line);
    }
    if !current.is_empty() {
        blocks.push(current);
    }

    let mut blocks = blocks.into_iter();
    let first = blocks.next().ok_or("Playlist is empty")?;
    if !first.contains("#EXTM3U") {
        return Err("Playlist is not valid".to_string());
    }

    // `x-tvg-url` (the EXTM3U header's XMLTV EPG source) isn't parsed -
    // this app fetches EPG data via TVHeadend's own JSON API
    // (`api/epg/events/grid`, see `src/epg.rs`), never via an external
    // XMLTV file.
    let mut items = Vec::new();

    for entry in blocks {
        let channel_name = {
            let name = get_attribute(&entry, "tvg-name");
            match name {
                Some(n) if !n.is_empty() => n,
                _ => get_name(&entry),
            }
        };
        let channel_id = get_attribute(&entry, "tvg-id").unwrap_or_default();
        let channel_number = get_attribute(&entry, "tvg-chno").unwrap_or_default();
        let logo_url = get_attribute(&entry, "tvg-logo")
            .filter(|s| !s.is_empty())
            .or_else(|| get_attribute(&entry, "logo"))
            .unwrap_or_default();
        let stream_url = get_attribute(&entry, "tvg-url")
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| get_url(&entry));

        items.push(M3uItem {
            channel_id,
            channel_number,
            channel_name,
            logo_url,
            stream_url,
        });
    }

    Ok(ParseResult { items })
}

/// Extract an `attr="value"` style attribute from an EXTINF line block.
fn get_attribute(entry: &str, name: &str) -> Option<String> {
    // e.g. tvg-id="978ffcc9bede159db867631b28b2ce0a"
    let needle = format!("{name}=\"");
    let lower_entry = entry.to_ascii_lowercase();
    let lower_needle = needle.to_ascii_lowercase();
    let start = lower_entry.find(&lower_needle)? + lower_needle.len();
    let rest = &entry[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The last non-tag, non-empty line of the block is the stream URL.
fn get_url(entry: &str) -> String {
    const SUPPORTED_TAGS: [&str; 3] = ["#EXTVLCOPT", "#EXTINF", "#EXTGRP"];
    entry
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| !SUPPORTED_TAGS.iter().any(|t| l.starts_with(t)))
        .last()
        .unwrap_or_default()
        .to_string()
}

/// Fallback channel name: text after the last comma on the #EXTINF line.
fn get_name(entry: &str) -> String {
    let first_line = entry.lines().next().unwrap_or(",");
    first_line
        .rsplit_once(',')
        .map(|(_, name)| name.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sample_playlist() {
        let sample = "#EXTM3U x-tvg-url=\"http://host:9981/xmltv/channels\"\n\
#EXTINF:-1 logo=\"http://host:9981/imagecache/7167?auth=abc\" tvg-id=\"978ffcc9bede\" tvg-chno=\"1\",Das Erste HD\n\
http://host:9981/stream/channelid/1241288599?auth=abc&profile=pass\n\
#EXTINF:-1 tvg-chno=\"2\",ZDF HD\n\
http://host:9981/stream/channelid/222?auth=abc&profile=pass\n";

        let result = parse(sample).unwrap();
        assert_eq!(result.items.len(), 2);
        assert_eq!(result.items[0].channel_name, "Das Erste HD");
        assert_eq!(result.items[0].channel_number, "1");
        assert_eq!(
            result.items[0].stream_url,
            "http://host:9981/stream/channelid/1241288599?auth=abc&profile=pass"
        );
        assert_eq!(result.items[1].channel_name, "ZDF HD");
    }

    #[test]
    fn rejects_invalid_playlist() {
        assert!(parse("not a playlist").is_err());
    }
}
