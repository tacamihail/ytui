//! yt-dlp integration.
//!
//! Listing runs `--flat-playlist --print`, which avoids resolving metadata for
//! every entry, and streams results to the UI line by line. Resolving asks for
//! the full JSON, picks the best AAC format that actually answers and hands the
//! URL to the decoder.

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use crate::config::Config;
use crate::entry::Entry;
use crate::remote;
use ytui_resolve::friendly;

pub(crate) use ytui_resolve::{command, tool_path, UA};

/// A resolve that was superseded by a newer selection. Callers drop it without
/// showing an error: nothing went wrong, the user moved on.
pub const CANCELLED: &str = ytui_resolve::CANCELLED;

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

#[derive(Debug)]
pub enum Msg {
    /// `seq` identifies the fetch that produced the message. A newer fetch
    /// bumps the counter, so a slow listing cannot append rows to a list that
    /// has already been replaced.
    Begin(u64, Kind),
    Add(u64, Kind, Entry),
    Done(u64, Kind, String),
    Error(u64, Kind, String),
}

const SEP: char = '\u{1f}';
const TEMPLATE: &str = "%(id)s\u{1f}%(title)s\u{1f}%(duration)s\u{1f}%(channel,uploader)s";

pub fn label(kind: Kind) -> &'static str {
    match kind {
        Kind::Search => "search",
        Kind::LikedMusic => "liked music",
        Kind::Playlists => "playlists",
        Kind::Subscriptions => "subscriptions",
        Kind::PlaylistTracks => "playlist",
    }
}

/// Runs a listing on a background thread, streaming entries through `tx`.
pub fn spawn(seq: u64, kind: Kind, target: String, cfg: Config, tx: Sender<Msg>) {
    thread::spawn(move || {
        let _ = tx.send(Msg::Begin(seq, kind));
        // The feed is a fixed tab, whatever target the caller passed.
        let target = if kind == Kind::Subscriptions {
            SUBSCRIPTIONS_URL.to_string()
        } else {
            target
        };
        let res = run(seq, kind, &target, &cfg, &tx).and_then(|n| {
            if kind == Kind::Subscriptions && n == 0 {
                Err("subscriptions feed came back empty, are you signed in?".into())
            } else {
                Ok(n)
            }
        });
        match res {
            Ok(n) => {
                let _ = tx.send(Msg::Done(seq, kind, format!("{}: {} items", label(kind), n)));
            }
            Err(e) => {
                let _ = tx.send(Msg::Error(seq, kind, e));
            }
        }
    });
}

/// `Some(true)` while a file-backed jar still signs in, `Some(false)` after
/// YouTube cleared the first-party cookies, `None` for browser cookies or an
/// unreadable file.
fn jar_signed_in(flag: &str, value: &str) -> Option<bool> {
    if flag != "--cookies" {
        return None;
    }
    std::fs::read_to_string(value)
        .ok()
        .map(|txt| ytui_resolve::has_session_cookies(&txt))
}

fn run(seq: u64, kind: Kind, target: &str, cfg: &Config, tx: &Sender<Msg>) -> Result<usize, String> {
    let mut c = command();
    c.args([
        "--flat-playlist",
        "--no-warnings",
        "--print",
        TEMPLATE,
        &format!("--playlist-items=1-{}", cfg.fetch_limit.max(1)),
    ]);
    // A feed is a single page, so lazy pagination buys nothing there.
    if kind != Kind::Subscriptions {
        // Print rows page by page instead of collecting the whole playlist
        // first: the first screen is on screen in about a second even when
        // the list has thousands of entries.
        c.arg("--lazy-playlist");
    }
    let cookies = cfg.cookie_args();
    // yt-dlp saves the jar when it exits, so only one run may touch it at a
    // time or a stale rotating token can overwrite a fresh one. A busy jar is
    // an error: running unlocked is the write this lock exists to prevent.
    let _cookie_lock = ytui_resolve::lock_cookies_for_run(
        cookies
            .as_ref()
            .map(|(flag, value)| (*flag, value.as_str())),
    )?;
    let before = cookies
        .as_ref()
        .and_then(|(flag, value)| jar_signed_in(flag, value));
    if let Some((flag, value)) = &cookies {
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
        if let Some(entry) = parse_line(kind, &line) {
            n += 1;
            let _ = tx.send(Msg::Add(seq, kind, entry));
        }
    }

    let status = child.wait().map_err(|e| e.to_string())?;
    let errtxt = errth.join().unwrap_or_default();
    // The run that gets the rejection is the one that writes the cleared jar,
    // so report it here instead of showing "playlist not found" forever.
    let after = cookies
        .as_ref()
        .and_then(|(flag, value)| jar_signed_in(flag, value));
    if before == Some(true) && after == Some(false) {
        return Err(ytui_resolve::SIGNED_OUT.to_string());
    }
    if !status.success() && n == 0 {
        Err(friendly(&errtxt))
    } else {
        Ok(n)
    }
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

fn parse_line(kind: Kind, line: &str) -> Option<Entry> {
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
    let url_override = if kind == Kind::Playlists {
        format!("https://www.youtube.com/playlist?list={id}")
    } else {
        String::new()
    };
    Some(Entry {
        id,
        title,
        channel,
        duration: parse_num(dur),
        url_override,
    })
}

fn parse_num(s: &str) -> u64 {
    if s.is_empty() || s == "NA" || s == "nan" {
        return 0;
    }
    s.split('.').next().unwrap_or("").parse().unwrap_or(0)
}

/// A playable stream plus the metadata yt-dlp already handed us, so an entry
/// created from a bare URL can show its real title instead of a placeholder.
#[derive(Clone, Debug)]
pub struct Stream {
    pub url: String,
    pub title: String,
    pub channel: String,
    pub duration: f64,
    /// True when this link came from a cache (the player's LRU or the
    /// resolver's) instead of a fresh yt-dlp run. The footer shows a marker
    /// for it, and nothing at all for a cold resolve.
    pub cached: bool,
}

/// Resolves a watch URL to a direct audio URL that our decoder can stream.
///
/// With `resolver_url` configured the work happens on another machine and the
/// candidates come back over HTTP. Either way the probes run locally, because
/// googlevideo URLs can be bound to the requesting IP. On a resolver failure
/// the client falls back to local yt-dlp unless `resolver_fallback` is off.
pub fn stream_url(url: &str, cfg: &Config) -> Result<Stream, String> {
    stream_url_cancelled(url, cfg, None)
}

/// Like [`stream_url`], but gives up as soon as `cancel` is raised. The local
/// yt-dlp child is killed; a remote resolver call cannot be interrupted once it
/// is in flight, so the flag is checked around it instead.
pub fn stream_url_cancelled(
    url: &str,
    cfg: &Config,
    cancel: Option<&AtomicBool>,
) -> Result<Stream, String> {
    if cancelled(cancel) {
        return Err(CANCELLED.into());
    }
    if let Some(settings) = remote::settings(cfg) {
        match remote_stream(url, &settings, cancel) {
            Ok(stream) => return Ok(stream),
            Err(e) if cancelled(cancel) => return Err(e),
            Err(e) if settings.fallback => remote::note_fallback(&e),
            Err(e) => return Err(format!("resolver: {e}")),
        }
    }
    local_stream(url, cfg, cancel)
}

/// True when the request was superseded by a newer one.
fn cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|c| c.load(Ordering::Relaxed))
}

fn remote_stream(
    url: &str,
    settings: &remote::Settings,
    cancel: Option<&AtomicBool>,
) -> Result<Stream, String> {
    let first = remote::resolve(settings, url, false)?;
    if let Ok(stream) = playable(first, cancel) {
        return Ok(stream);
    }
    if cancelled(cancel) {
        return Err(CANCELLED.into());
    }
    // A cached URL can expire while the client still has it. Ask once more
    // with the server cache bypassed before giving up.
    let refreshed = remote::resolve(settings, url, true)?;
    playable(refreshed, cancel).map_err(|e| format!("refreshed URLs still failed ({e})"))
}

fn local_stream(
    url: &str,
    cfg: &Config,
    cancel: Option<&AtomicBool>,
) -> Result<Stream, String> {
    let cookies = cfg.cookie_args();
    let resolved = ytui_resolve::resolve_cancellable(
        url,
        cookies
            .as_ref()
            .map(|(flag, value)| (*flag, value.as_str())),
        cancel,
    )?;
    playable(resolved, cancel)
}

/// Probes the candidates best-first and returns the first one the CDN serves.
fn playable(resolved: ytui_resolve::Resolved, cancel: Option<&AtomicBool>) -> Result<Stream, String> {
    let mut stream = Stream {
        url: String::new(),
        title: resolved.title,
        channel: resolved.channel,
        duration: resolved.duration,
        cached: resolved.cached,
    };
    let mut last = String::from("no candidate answered");
    for candidate in resolved.candidates.iter().take(4) {
        if cancelled(cancel) {
            return Err(CANCELLED.into());
        }
        match probe(&candidate.url) {
            Ok(()) => {
                stream.url = candidate.url.clone();
                return Ok(stream);
            }
            Err(e) => last = e,
        }
    }
    Err(format!("YouTube refused every AAC stream ({last})"))
}

/// Cheap liveness check for a googlevideo URL.
fn probe(url: &str) -> Result<(), String> {
    match ureq::get(url)
        .set("Range", "bytes=0-1023")
        .set("User-Agent", UA)
        .timeout(Duration::from_secs(12))
        .call()
    {
        Ok(resp) => {
            let code = resp.status();
            if code == 200 || code == 206 {
                Ok(())
            } else {
                Err(format!("http {code}"))
            }
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Reads the signed-in account's channel name and id from a system playlist
/// that belongs to the user.
///
/// Watch Later carries the owner in `channel`/`channel_id`; the Liked Music
/// playlist (`LM`) is returned with those fields null, so it only serves as a
/// fallback for accounts that cannot use Watch Later.
pub fn account(cfg: &Config) -> Result<(String, String), String> {
    let Some((flag, value)) = cfg.cookie_args() else {
        return Err("no cookies configured".into());
    };
    // Same jar as the player's listings and resolves; serialize with them.
    let _cookie_lock = ytui_resolve::lock_cookies_for_run(Some((flag, value.as_str())))?;
    let before = jar_signed_in(flag, &value);
    let mut last_err = String::new();
    for url in [WATCH_LATER_URL, LIKED_MUSIC_URL] {
        let out = command()
            .args([
                "--flat-playlist",
                "--playlist-items",
                "1",
                "-J",
                "--no-warnings",
                flag,
                &value,
                url,
            ])
            .stdin(Stdio::null())
            .output()
            .map_err(|_| "yt-dlp not found".to_string())?;
        if !out.status.success() {
            last_err = friendly(&String::from_utf8_lossy(&out.stderr));
            continue;
        }
        let v: serde_json::Value = match serde_json::from_slice(&out.stdout) {
            Ok(v) => v,
            Err(e) => {
                last_err = format!("bad yt-dlp json: {e}");
                continue;
            }
        };
        let name = v
            .get("channel")
            .and_then(|x| x.as_str())
            .or_else(|| v.get("uploader").and_then(|x| x.as_str()))
            .unwrap_or("")
            .to_string();
        let id = v
            .get("channel_id")
            .and_then(|x| x.as_str())
            .or_else(|| v.get("uploader_id").and_then(|x| x.as_str()))
            .unwrap_or("")
            .to_string();
        if !name.is_empty() || !id.is_empty() {
            return Ok((name, id));
        }
    }
    if last_err.is_empty() {
        Err("could not read the account: cookies are accepted but YouTube returned no owner".into())
    } else if before == Some(true) && jar_signed_in(flag, &value) == Some(false) {
        Err(ytui_resolve::SIGNED_OUT.to_string())
    } else {
        Err(last_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

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

    /// A resolver and a CDN in one: `/v1/resolve` returns a candidate, and the
    /// candidate URL answers the 1 KiB probe with 206.
    fn fake_resolver() -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            for _ in 0..2 {
                let Ok((mut sock, _)) = listener.accept() else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let n = sock.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let (status, body) = if request.starts_with("GET /v1/resolve") {
                    let body = format!(
                        r#"{{"v":1,"id":"dQw4w9WgXcQ","title":"Remote","channel":"Server","duration":1.0,"ms":3,"candidates":[{{"url":"http://127.0.0.1:{}/audio","itag":"140","abr":128}}]}}"#,
                        addr.port()
                    );
                    ("200 OK", body)
                } else if request.starts_with("GET /audio") {
                    ("206 Partial Content", String::new())
                } else {
                    ("404 Not Found", String::new())
                };
                let _ = write!(
                    sock,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[test]
    fn remote_resolver_supplies_a_playable_candidate() {
        let (base, server) = fake_resolver();
        let cfg = Config {
            resolver_url: base,
            resolver_fallback: false,
            ..Config::default()
        };
        let stream = stream_url("https://youtu.be/dQw4w9WgXcQ", &cfg).unwrap();
        assert_eq!(stream.title, "Remote");
        assert_eq!(stream.channel, "Server");
        assert!(stream.url.ends_with("/audio"), "{}", stream.url);
        server.join().unwrap();
    }
}
