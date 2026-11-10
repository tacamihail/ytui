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

/// Identifies the signed-in account without opening the cookie jar.
///
/// yt-dlp rewrites the `--cookies` file after every run (it refreshes the
/// rotating `__Secure-1PSIDTS`), so the file's size and mtime change all the
/// time. Only the first-party session cookies say *whose* lists these are, and
/// they change exactly when the account does or when a fresh export replaces
/// the old session. Browser cookies have no file to read, so the browser name
/// stands in for them.
pub fn stamp(cfg: &Config) -> String {
    if let Some(path) = cfg.cookie_path() {
        return session_stamp(&path);
    }
    if !cfg.cookies_browser.is_empty() {
        return format!("browser:{}", cfg.cookies_browser);
    }
    "none".into()
}

/// Cookies that identify the account rather than a visitor or a rotating
/// token. Their values change on re-import or when a different account is
/// signed in, which is exactly when the cached lists must be discarded.
const SESSION_COOKIES: &[&str] = &[
    "SID",
    "HSID",
    "SSID",
    "APISID",
    "SAPISID",
    "__Secure-1PSID",
    "__Secure-1PAPISID",
    "__Secure-3PSID",
    "__Secure-3PAPISID",
];

fn session_stamp(path: &Path) -> String {
    let Ok(txt) = fs::read_to_string(path) else {
        return "file:missing".into();
    };
    // FNV-1a over just the names and values, so the result is stable across
    // runs and builds (unlike DefaultHasher).
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for line in txt.lines() {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') {
            continue;
        }
        let mut f = line.split('\t');
        let (Some(_domain), Some(_flag), Some(_path), Some(_secure), Some(_exp), Some(name), Some(value)) = (
            f.next(),
            f.next(),
            f.next(),
            f.next(),
            f.next(),
            f.next(),
            f.next(),
        ) else {
            continue;
        };
        if !SESSION_COOKIES.contains(&name) {
            continue;
        }
        for b in name.bytes().chain(std::iter::once(b'=')).chain(value.bytes()) {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
    }
    format!("auth:{hash:016x}")
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

    fn cookie_file(dir: &Path, lines: &[(&str, &str)]) -> Config {
        fs::create_dir_all(dir).unwrap();
        let p = dir.join("cookies.txt");
        let mut txt = String::from("# Netscape HTTP Cookie File\n");
        for &(name, value) in lines {
            let prefix = if name == "SID" { "#HttpOnly_" } else { "" };
            txt.push_str(&format!(
                "{prefix}.youtube.com\tTRUE\t/\tTRUE\t1900000000\t{name}\t{value}\n"
            ));
        }
        fs::write(&p, txt).unwrap();
        Config {
            cookies_file: p.display().to_string(),
            ..Config::default()
        }
    }

    /// yt-dlp refreshes the rotating tokens after every run. That must not
    /// throw away the cached lists; a different session must.
    #[test]
    fn the_stamp_follows_the_session_not_the_rotating_cookies() {
        let dir = tmp_dir();
        let a = stamp(&cookie_file(
            &dir,
            &[("SID", "one"), ("__Secure-1PSIDTS", "old")],
        ));
        let b = stamp(&cookie_file(
            &dir,
            &[("SID", "one"), ("__Secure-1PSIDTS", "new")],
        ));
        assert_eq!(a, b);
        let c = stamp(&cookie_file(
            &dir,
            &[("SID", "two"), ("__Secure-1PSIDTS", "new")],
        ));
        assert_ne!(a, c);
        assert!(a.starts_with("auth:"), "{a}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_cookie_file_has_no_stamp() {
        let cfg = Config {
            cookies_file: tmp_dir().join("gone.txt").display().to_string(),
            ..Config::default()
        };
        assert_eq!(stamp(&cfg), "none");
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
