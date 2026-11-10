//! A copy of the last successful listing, so a tab paints immediately.
//!
//! Fetching a playlist means waiting a second or two for YouTube before the
//! first row can be drawn. Keeping the rows on disk turns that wait into a
//! background refresh: the cached list is shown at once and replaced when the
//! new page arrives. Search results are not cached; they are cheap and a stale
//! result set is confusing.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::config::{self, Config};
use crate::entry::Entry;
use crate::ytdl::Kind;

/// How long a cached list is trusted without a network refresh. Older rows are
/// still painted, but a refresh starts behind them.
const TTL_SECS: u64 = 15 * 60;

/// Bumped when a cache written by an older build is known to be incomplete, so
/// it is painted once and then refreshed instead of trusted for another TTL.
/// Version 2: 0.4.5 and older could stop a listing at the first non-UTF-8
/// byte (a cp1252 curly quote on Windows) and then cache the short list.
const CACHE_VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
pub struct Cached {
    /// Missing in caches written before the field existed; those count as 0.
    #[serde(default)]
    version: u32,
    saved_at: u64,
    /// Cookie-file fingerprint from [`stamp`]; rows from another account are
    /// never shown.
    stamp: String,
    pub entries: Vec<Entry>,
}

impl Cached {
    pub fn fresh(&self) -> bool {
        self.version == CACHE_VERSION && now().saturating_sub(self.saved_at) < TTL_SECS
    }
}

/// On-disk name for a list, or `None` when the list is not worth caching.
pub fn key(kind: Kind) -> Option<&'static str> {
    match kind {
        Kind::LikedMusic => Some("liked"),
        Kind::Playlists => Some("playlists"),
        Kind::Subscriptions => Some("subs"),
        Kind::Search | Kind::PlaylistTracks => None,
    }
}

/// Identifies the signed-in account without opening the cookie jar: the file's
/// size and mtime change whenever cookies are re-exported or removed. Browser
/// cookies have no file to stat, so the browser name stands in for them.
pub fn stamp(cfg: &Config) -> String {
    if let Some(path) = cfg.cookie_path() {
        return file_stamp(&path);
    }
    if !cfg.cookies_browser.is_empty() {
        return format!("browser:{}", cfg.cookies_browser);
    }
    "none".into()
}

fn file_stamp(path: &Path) -> String {
    match fs::metadata(path) {
        Ok(m) => {
            let mtime = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("file:{}:{}", m.len(), mtime)
        }
        Err(_) => "file:missing".into(),
    }
}

pub fn load(key: &str, stamp: &str) -> Option<Cached> {
    load_from(&cache_dir(), key, stamp)
}

pub fn save(key: &str, stamp: &str, entries: &[Entry]) {
    save_to(&cache_dir(), key, stamp, entries);
}

pub fn clear() {
    let _ = fs::remove_dir_all(cache_dir());
}

fn cache_dir() -> PathBuf {
    config::config_dir().join("cache")
}

fn load_from(dir: &Path, key: &str, stamp: &str) -> Option<Cached> {
    let mut c: Cached = config::load_json(&dir.join(format!("{key}.json")))?;
    if c.stamp != stamp {
        return None;
    }
    // Older builds could cache feed rows that had no title: a bare `videoId`
    // from a shorts shelf or a menu command was mistaken for a video, and those
    // rows painted as blank lines. Drop them instead of showing holes.
    c.entries.retain(|e| !e.title.trim().is_empty());
    Some(c)
}

fn save_to(dir: &Path, key: &str, stamp: &str, entries: &[Entry]) {
    let c = Cached {
        version: CACHE_VERSION,
        saved_at: now(),
        stamp: stamp.to_string(),
        entries: entries.to_vec(),
    };
    config::save_json(&dir.join(format!("{key}.json")), &c);
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> Entry {
        Entry {
            id: id.into(),
            title: format!("track {id}"),
            channel: "channel".into(),
            duration: 200,
            url_override: String::new(),
        }
    }

    fn tmp_dir() -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        std::env::temp_dir().join(format!("ytui-listcache-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn lists_survive_a_round_trip() {
        let dir = tmp_dir();
        save_to(&dir, "liked", "file:10:20", &[entry("a"), entry("b")]);
        let c = load_from(&dir, "liked", "file:10:20").expect("cache should load");
        assert_eq!(c.entries.len(), 2);
        assert!(c.fresh());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn another_accounts_rows_are_ignored() {
        let dir = tmp_dir();
        save_to(&dir, "liked", "file:10:20", &[entry("a")]);
        assert!(load_from(&dir, "liked", "file:11:21").is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Caches written by older builds can contain rows whose title never got
    /// parsed. They used to show up as blank lines in the lists.
    #[test]
    fn untitled_rows_are_dropped_on_load() {
        let dir = tmp_dir();
        let mut blank = entry("b");
        blank.title = "  ".into();
        save_to(&dir, "subs", "file:10:20", &[entry("a"), blank]);
        let c = load_from(&dir, "subs", "file:10:20").expect("cache should load");
        assert_eq!(c.entries.len(), 1);
        assert_eq!(c.entries[0].id, "a");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_lists_are_stale_but_still_usable() {
        let c = Cached {
            version: CACHE_VERSION,
            saved_at: now() - TTL_SECS - 1,
            stamp: "none".into(),
            entries: Vec::new(),
        };
        assert!(!c.fresh());
    }

    /// A cache from a build that could truncate a listing is painted once, but
    /// must not suppress the refresh that replaces its missing rows.
    #[test]
    fn caches_from_older_builds_refresh_at_once() {
        let dir = tmp_dir();
        let old = format!(r#"{{"saved_at":{},"stamp":"none","entries":[]}}"#, now());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("liked.json"), old).unwrap();
        let c = load_from(&dir, "liked", "none").expect("old cache should still parse");
        assert!(!c.fresh());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_account_lists_are_cached() {
        assert_eq!(key(Kind::LikedMusic), Some("liked"));
        assert_eq!(key(Kind::Playlists), Some("playlists"));
        assert_eq!(key(Kind::Subscriptions), Some("subs"));
        assert_eq!(key(Kind::Search), None);
        assert_eq!(key(Kind::PlaylistTracks), None);
    }
}
