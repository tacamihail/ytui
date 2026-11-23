//! Playlist, feed and search listings.
//!
//! Listing runs `--flat-playlist --print`, which avoids resolving metadata
//! for every entry, and streams the rows to the caller as yt-dlp prints
//! them. The TUI uses that for its progressive fill; the resolver's web API
//! collects them into one JSON answer. Both share the cookie-jar lock and
//! the signed-out detection, so a listing can never race a resolve on the
//! same jar.

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::thread;

use serde::{Deserialize, Serialize};

use crate::{command, friendly, jar_session_state, lock_cookies_for_run, SIGNED_OUT};

pub const LIKED_MUSIC_URL: &str = "https://music.youtube.com/playlist?list=LM";
/// System playlist owned by the account; unlike `LM` it exposes the owner.
pub const WATCH_LATER_URL: &str = "https://www.youtube.com/playlist?list=WL";
/// The subscriptions feed. yt-dlp has an extractor for this tab, and using it
/// (instead of fetching the page with a raw `Cookie:` header) is important:
/// YouTube rotates `__Secure-1PSIDTS` on responses, and a reader that throws
/// that rotation away ages the stored token until it is treated as replayed.
pub const SUBSCRIPTIONS_URL: &str = "https://www.youtube.com/feed/subscriptions";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Search,
    LikedMusic,
    Playlists,
    Subscriptions,
    PlaylistTracks,
}

/// One row of a listing: what yt-dlp's `--flat-playlist --print` template
/// hands back for every entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrackMeta {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub duration: u64,
}

pub fn label(kind: Kind) -> &'static str {
    match kind {
        Kind::Search => "search",
        Kind::LikedMusic => "liked music",
        Kind::Playlists => "playlists",
        Kind::Subscriptions => "subscriptions",
        Kind::PlaylistTracks => "playlist",
    }
}

const SEP: char = '\u{1f}';
const TEMPLATE: &str = "%(id)s\u{1f}%(title)s\u{1f}%(duration)s\u{1f}%(channel,uploader)s";

/// Runs a listing and hands every row to `on_entry` as yt-dlp prints it.
///
/// `cookies` is yt-dlp's `--cookies <path>` / `--cookies-from-browser <name>`
/// pair. A file-backed jar is locked for the whole run, and the signed-out
/// transition YouTube answers with is reported as [`SIGNED_OUT`] instead of a
/// confusing "playlist not found". Subscriptions is a fixed target, whatever
/// the caller passed.
pub fn list(
    kind: Kind,
    target: &str,
    fetch_limit: u32,
    cookies: Option<(&str, &str)>,
    on_entry: &mut dyn FnMut(TrackMeta),
) -> Result<usize, String> {
    // The feed is a fixed tab, whatever target the caller passed.
    let target = if kind == Kind::Subscriptions {
        SUBSCRIPTIONS_URL
    } else {
        target
    };
    let mut c = command();
    c.args([
        "--flat-playlist",
        "--no-warnings",
        "--print",
        TEMPLATE,
        &format!("--playlist-items=1-{}", fetch_limit.max(1)),
    ]);
    // A feed is a single page, so lazy pagination buys nothing there.
    if kind != Kind::Subscriptions {
        // Print rows page by page instead of collecting the whole playlist
        // first: the first screen is on screen in about a second even when
        // the list has thousands of entries.
        c.arg("--lazy-playlist");
    }
    // yt-dlp saves the jar when it exits, so only one run may touch it at a
    // time or a stale rotating token can overwrite a fresh one. A busy jar is
    // an error: running unlocked is the write this lock exists to prevent.
    let _cookie_lock = lock_cookies_for_run(cookies)?;
    let before = jar_session_state(cookies);
    if let Some((flag, value)) = cookies {
        c.arg(flag).arg(value);
    }
    c.arg(target);
    c.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());

    let mut child = c
        .spawn()
        .map_err(|_| "yt-dlp not found, install it and run: ytui doctor".to_string())?;

    let out = child.stdout.take().unwrap();
    let err = child.stderr.take().unwrap();
    let errth = thread::spawn(move || {
        let mut buf = String::new();
        for line in lossy_lines(BufReader::new(err)) {
            if buf.lines().count() < 6 {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
        buf
    });

    let mut n = 0usize;
    for line in lossy_lines(BufReader::new(out)) {
        if let Some(entry) = parse_line(&line) {
            n += 1;
            on_entry(entry);
        }
    }

    let status = child.wait().map_err(|e| e.to_string())?;
    let errtxt = errth.join().unwrap_or_default();
    // The run that gets the rejection is the one that writes the cleared jar,
    // so report it here instead of showing "playlist not found" forever.
    let after = jar_session_state(cookies);
    if before == Some(true) && after == Some(false) {
        return Err(SIGNED_OUT.into());
    }
    if !status.success() && n == 0 {
        Err(friendly(&errtxt))
    } else {
        Ok(n)
    }
}

fn parse_line(line: &str) -> Option<TrackMeta> {
    let mut it = line.split(SEP);
    let id = it.next()?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let title = it.next().unwrap_or("").trim().to_string();
    let dur = it.next().unwrap_or("").trim();
    let channel = it.next().unwrap_or("").trim().to_string();
    let title = if title.is_empty() {
        format!("untitled {id}")
    } else {
        title
    };
    Some(TrackMeta {
        id,
        title,
        channel,
        duration: parse_num(dur),
    })
}

fn parse_num(s: &str) -> u64 {
    if s.is_empty() || s == "NA" || s == "nan" {
        return 0;
    }
    s.split('.').next().unwrap_or("").parse().unwrap_or(0)
}

/// Reads a pipe line by line, decoding each line separately and replacing a
/// byte that is not UTF-8 instead of refusing the whole stream.
///
/// `BufRead::lines()` returns `Err` on the first invalid byte. Callers that
/// stopped at that error (`map_while(Result::ok)`) used to drop the read end of
/// the pipe in the middle of a listing; yt-dlp then failed with
/// `[Errno 22] Invalid argument` and the list ended at a song title with a
/// curly quote. `yt-dlp --encoding utf-8` is set in [`command`] so this is a
/// safety net for warnings and for other tools, not the happy path.
fn lossy_lines<R: BufRead>(mut reader: R) -> impl Iterator<Item = String> {
    std::iter::from_fn(move || {
        let mut buf = Vec::new();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => None,
            Ok(_) => {
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                }
                if buf.last() == Some(&b'\r') {
                    buf.pop();
                }
                Some(String::from_utf8_lossy(&buf).into_owned())
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 31st liked song carried a cp1252 curly quote, and stopping at that
    /// byte truncated the rest of the list. Every line must survive.
    #[test]
    fn lossy_lines_keep_going_past_an_invalid_byte() {
        let raw: &[u8] = b"first\nsecond\nArun\x92s Roof\nfourth\n";
        let lines: Vec<String> = lossy_lines(raw).collect();
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "first");
        assert_eq!(lines[2], "Arun\u{fffd}s Roof");
        assert_eq!(lines[3], "fourth");
    }

    #[test]
    fn lossy_lines_strip_windows_newlines() {
        let lines: Vec<String> = lossy_lines(&b"a\r\nb\r\n"[..]).collect();
        assert_eq!(lines, ["a", "b"]);
    }

    #[test]
    fn parses_a_template_row() {
        let row = format!("id1{SEP}Song{SEP}213.37{SEP}Artist");
        let m = parse_line(&row).unwrap();
        assert_eq!(m.id, "id1");
        assert_eq!(m.title, "Song");
        assert_eq!(m.channel, "Artist");
        assert_eq!(m.duration, 213);
    }

    #[test]
    fn an_empty_title_falls_back_to_the_id() {
        let row = format!("id2{SEP}{SEP}NA{SEP}");
        let m = parse_line(&row).unwrap();
        assert_eq!(m.title, "untitled id2");
        assert_eq!(m.duration, 0);
    }

    #[test]
    fn an_empty_id_is_not_a_row() {
        let row = format!("{SEP}Song{SEP}213{SEP}Artist");
        assert!(parse_line(&row).is_none());
    }
}
