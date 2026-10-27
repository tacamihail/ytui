//! The one thing yt-dlp cannot give us: the subscriptions feed.
//!
//! `https://www.youtube.com/feed/subscriptions` is rendered server-side with the
//! account's cookies; there is no extractor for it. We fetch the page and walk
//! `ytInitialData`, collecting anything that looks like a video (old
//! `videoRenderer` nodes and the newer `lockupViewModel` ones).

use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::config::Config;
use crate::entry::Entry;
use crate::ytdl::UA;

pub fn subscription_feed(cfg: &Config) -> Result<Vec<Entry>, String> {
    let path = cfg
        .cookie_path()
        .ok_or_else(|| "subscriptions need cookies.txt — run: ytui auth <file>".to_string())?;
    let cookie = cookie_header(&path)
        .ok_or_else(|| "cookies.txt contains no youtube.com cookies".to_string())?;

    let body = ureq::get("https://www.youtube.com/feed/subscriptions")
        .set("User-Agent", UA)
        .set("Cookie", &cookie)
        .set("Accept-Language", "en-US,en;q=0.9")
        .timeout(Duration::from_secs(25))
        .call()
        .map_err(|e| format!("subscriptions feed: {e}"))?
        .into_string()
        .map_err(|e| format!("subscriptions feed: {e}"))?;

    let data = extract_json(&body, "ytInitialData")
        .ok_or_else(|| "could not find ytInitialData in the subscriptions page".to_string())?;

    let mut out = Vec::new();
    collect(&data, &mut out);
    dedup(&mut out);

    if out.is_empty() {
        return Err("subscriptions feed came back empty — are you signed in?".into());
    }
    out.truncate(cfg.fetch_limit.max(1) as usize);
    Ok(out)
}

/// Builds a `Cookie:` header from a Netscape cookies.txt.
pub fn cookie_header(path: &Path) -> Option<String> {
    let txt = std::fs::read_to_string(path).ok()?;
    let mut pairs: Vec<String> = Vec::new();
    for line in txt.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 7 {
            continue;
        }
        let domain = f[0];
        if !domain.contains("youtube.com") && !domain.contains("google.com") {
            continue;
        }
        pairs.push(format!("{}={}", f[5], f[6]));
    }
    if pairs.is_empty() {
        None
    } else {
        Some(pairs.join("; "))
    }
}

/// Finds `var <name> = {...};` and returns the parsed object.
fn extract_json(html: &str, name: &str) -> Option<Value> {
    let at = html.find(name)?;
    let start = html[at..].find('{')? + at;
    let bytes = html.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&html[start..=i]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

fn collect(v: &Value, out: &mut Vec<Entry>) {
    match v {
        Value::Object(map) => {
            if let Some(id) = map.get("videoId").and_then(Value::as_str) {
                if is_video_id(id) {
                    let title = map
                        .get("title")
                        .or_else(|| map.get("headline"))
                        .and_then(text_of)
                        .unwrap_or_default();
                    let channel = map
                        .get("ownerText")
                        .or_else(|| map.get("longBylineText"))
                        .or_else(|| map.get("shortBylineText"))
                        .and_then(text_of)
                        .unwrap_or_default();
                    let duration = map
                        .get("lengthText")
                        .and_then(text_of)
                        .map(|s| parse_duration(&s))
                        .unwrap_or(0);
                    out.push(Entry {
                        id: id.to_string(),
                        title,
                        channel,
                        duration,
                        url_override: String::new(),
                    });
                }
            }
            if let Some(lockup) = map.get("lockupViewModel") {
                if let Some(e) = from_lockup(lockup) {
                    out.push(e);
                }
            }
            for child in map.values() {
                collect(child, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect(item, out);
            }
        }
        _ => {}
    }
}

/// Newer YouTube UI: `lockupViewModel` with a `contentId` and nested metadata.
fn from_lockup(lockup: &Value) -> Option<Entry> {
    let content_type = lockup
        .get("contentType")
        .and_then(Value::as_str)
        .unwrap_or("");
    if !content_type.contains("VIDEO") {
        return None;
    }
    let id = lockup.get("contentId").and_then(Value::as_str)?;
    if !is_video_id(id) {
        return None;
    }
    let meta = lockup
        .get("metadata")
        .and_then(|m| m.get("lockupMetadataViewModel"));
    let title = meta
        .and_then(|m| m.get("title"))
        .and_then(text_of)
        .unwrap_or_default();

    let mut channel = String::new();
    let mut duration = 0u64;
    if let Some(parts) = meta
        .and_then(|m| m.get("metadata"))
        .and_then(|m| m.get("contentMetadataViewModel"))
        .and_then(|m| m.get("metadataRows"))
        .and_then(Value::as_array)
    {
        let mut texts: Vec<String> = Vec::new();
        for row in parts {
            if let Some(p) = row.get("metadataParts").and_then(Value::as_array) {
                for part in p {
                    if let Some(t) = part.get("text").and_then(text_of) {
                        texts.push(t);
                    }
                }
            }
        }
        if let Some(first) = texts.first() {
            channel = first.clone();
        }
        if let Some(last) = texts.iter().rev().find(|t| t.contains(':')) {
            duration = parse_duration(last);
        }
    }

    Some(Entry {
        id: id.to_string(),
        title,
        channel,
        duration,
        url_override: String::new(),
    })
}

fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Object(m) => {
            if let Some(runs) = m.get("runs").and_then(Value::as_array) {
                let s: String = runs
                    .iter()
                    .filter_map(|r| r.get("text").and_then(Value::as_str))
                    .collect();
                if !s.is_empty() {
                    return Some(s);
                }
            }
            if let Some(s) = m.get("simpleText").and_then(Value::as_str) {
                return Some(s.to_string());
            }
            if let Some(s) = m.get("content").and_then(Value::as_str) {
                return Some(s.to_string());
            }
            None
        }
        _ => None,
    }
}

fn is_video_id(s: &str) -> bool {
    s.len() == 11
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn parse_duration(s: &str) -> u64 {
    let mut secs = 0u64;
    let mut parts = 0;
    for p in s.trim().split(':') {
        let Ok(n) = p.parse::<u64>() else {
            return 0;
        };
        secs = secs * 60 + n;
        parts += 1;
    }
    if parts == 0 {
        0
    } else {
        secs
    }
}

fn dedup(items: &mut Vec<Entry>) {
    let mut seen = std::collections::HashSet::new();
    items.retain(|e| seen.insert(e.id.clone()));
}
