//! yt-dlp integration.
//!
//! Listing runs `--flat-playlist --print`, which avoids resolving metadata for
//! every entry, and streams results to the UI line by line. Resolving asks for
//! the full JSON, picks the best AAC format that actually answers and hands the
//! URL to the decoder.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use crate::config::Config;
use crate::entry::Entry;

pub const LIKED_MUSIC_URL: &str = "https://music.youtube.com/playlist?list=LM";
/// System playlist owned by the account; unlike `LM` it exposes the owner.
pub const WATCH_LATER_URL: &str = "https://www.youtube.com/playlist?list=WL";

/// Browser-ish UA; googlevideo accepts any modern string.
pub const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                      Chrome/126.0.0.0 Safari/537.36";

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

/// `yt-dlp` plus the directories the installer uses, so a binary installed by
/// `install.sh` is found even when the TUI was started from a shell that has
/// not picked up the new PATH yet. The same PATH also lets yt-dlp find `deno`,
/// which it needs to solve YouTube's JS challenges.
pub(crate) fn tool_path() -> String {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    for var in ["HOME", "USERPROFILE"] {
        if let Ok(home) = std::env::var(var) {
            if !home.is_empty() {
                let home = std::path::PathBuf::from(home);
                dirs.push(home.join(".local/bin"));
                dirs.push(home.join(".deno/bin"));
                dirs.push(home.join(".cargo/bin"));
                break;
            }
        }
    }
    if let Ok(path) = std::env::var("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(dirs)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|d| d.join(name).is_file()))
        .unwrap_or(false)
}

pub(crate) fn command() -> Command {
    // Resolving a track is background work, but yt-dlp + deno still burn a
    // core for a couple of seconds. Running it below the audio thread's
    // priority keeps a busy prefetch from turning into the occasional click.
    let mut c = if on_path("nice") && on_path("yt-dlp") {
        let mut c = Command::new("nice");
        c.args(["-n", "10", "yt-dlp"]);
        c
    } else {
        Command::new("yt-dlp")
    };
    c.env("PATH", tool_path());
    c
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

/// Runs a listing on a background thread, streaming entries through `tx`.
pub fn spawn(seq: u64, kind: Kind, target: String, cfg: Config, tx: Sender<Msg>) {
    thread::spawn(move || {
        let _ = tx.send(Msg::Begin(seq, kind));
        let res = match kind {
            Kind::Subscriptions => crate::ytapi::subscription_feed(&cfg).map(|list| {
                let n = list.len();
                for e in list {
                    let _ = tx.send(Msg::Add(seq, kind, e));
                }
                n
            }),
            _ => run(seq, kind, &target, &cfg, &tx),
        };
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

fn run(seq: u64, kind: Kind, target: &str, cfg: &Config, tx: &Sender<Msg>) -> Result<usize, String> {
    let mut c = command();
    c.args([
        "--flat-playlist",
        // Print rows page by page instead of collecting the whole playlist
        // first: the first screen is on screen in about a second even when
        // the list has thousands of entries.
        "--lazy-playlist",
        "--no-warnings",
        "--print",
        TEMPLATE,
        &format!("--playlist-items=1-{}", cfg.fetch_limit.max(1)),
    ]);
    if let Some((flag, value)) = cfg.cookie_args() {
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
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            if buf.lines().count() < 6 {
                buf.push_str(&line);
                buf.push('\n');
            }
        }
        buf
    });

    let mut n = 0usize;
    for line in BufReader::new(out).lines().map_while(Result::ok) {
        if let Some(entry) = parse_line(kind, &line) {
            n += 1;
            let _ = tx.send(Msg::Add(seq, kind, entry));
        }
    }

    let status = child.wait().map_err(|e| e.to_string())?;
    let errtxt = errth.join().unwrap_or_default();
    if !status.success() && n == 0 {
        Err(friendly(&errtxt))
    } else {
        Ok(n)
    }
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
}

/// Resolves a watch URL to a direct audio URL that our decoder can stream.
///
/// YouTube is inconsistent about which AAC bitrates it serves: `itag=140`
/// (128 kbps) works for most videos but returns 403 for others, while
/// `itag=139` (HE-AAC, 48 kbps) almost always works. So instead of trusting
/// the first URL `-g` prints, we ask for the whole JSON, order the AAC formats
/// best-first and probe each with a 1 KiB Range request until one answers.
pub fn stream_url(url: &str, cfg: &Config) -> Result<Stream, String> {
    let mut c = command();
    c.args(["--no-warnings", "-j", url]);
    if let Some((flag, value)) = cfg.cookie_args() {
        c.arg(flag).arg(value);
    }
    let out = c
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "yt-dlp not found, install it and run: ytui doctor".to_string())?;
    if !out.status.success() {
        return Err(friendly(&String::from_utf8_lossy(&out.stderr)));
    }
    let info: serde_json::Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("bad yt-dlp json: {e}"))?;

    let text = |key: &str| {
        info.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let meta = Stream {
        url: String::new(),
        title: text("title"),
        channel: {
            let channel = text("channel");
            if channel.is_empty() {
                text("uploader")
            } else {
                channel
            }
        },
        duration: info.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0),
    };

    let formats = info
        .get("formats")
        .and_then(|f| f.as_array())
        .ok_or_else(|| "yt-dlp returned no formats".to_string())?;

    let mut candidates: Vec<(u8, u64, String)> = Vec::new();
    for f in formats {
        let Some(u) = f.get("url").and_then(|v| v.as_str()) else {
            continue;
        };
        let ext = f.get("ext").and_then(|v| v.as_str()).unwrap_or("");
        let codec = f.get("acodec").and_then(|v| v.as_str()).unwrap_or("");
        if ext != "m4a" && !codec.starts_with("mp4a") {
            continue;
        }
        let id = f.get("format_id").and_then(|v| v.as_str()).unwrap_or("");
        // Lower rank = preferred. 141 is 256 kbps (Music Premium), 140 is
        // 128 kbps, 139 is 48 kbps HE-AAC.
        let rank = match id {
            "141" => 0,
            "140" => 1,
            "139" => 2,
            _ => 3,
        };
        let abr = f.get("abr").and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
        candidates.push((rank, u64::MAX - abr.min(1_000_000) as u64, u.to_string()));
    }
    if candidates.is_empty() {
        return Err("no AAC stream for this track (opus only)".into());
    }
    candidates.sort_by_key(|(rank, abr, _)| (*rank, *abr));

    let mut last = String::from("no candidate answered");
    let mut stream = meta;
    for (_, _, candidate) in candidates.iter().take(4) {
        match probe(candidate) {
            Ok(()) => {
                stream.url = candidate.clone();
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
    } else {
        Err(last_err)
    }
}

fn friendly(stderr: &str) -> String {
    let low = stderr.to_lowercase();
    if low.contains("in your country") {
        "blocked in your country".into()
    } else if low.contains("video unavailable") || low.contains("this video is not available") {
        // YouTube answers this for removed uploads, region blocks and tracks
        // that only exist in its catalogue. Nothing about it is fixable here.
        "unavailable on youtube".into()
    } else if low.contains("not a bot") {
        "youtube wants a sign-in check, try again later".into()
    } else if low.contains("confirm your age") || low.contains("age-restricted") {
        "age-restricted, the cookies must be from an adult account".into()
    } else if low.contains("members-only") || low.contains("join this channel") {
        "members-only, this account cannot play it".into()
    } else if low.contains("playlist does not exist") {
        // YouTube answers this for Watch Later / Liked Music when the cookies
        // are signed out or missing their HttpOnly half.
        "playlist not found, cookies may have expired: run `ytui auth <cookies.txt>`".into()
    } else if low.contains("private") || low.contains("sign in") || low.contains("login") {
        "private: sign-in required, run `ytui auth <cookies.txt>`".into()
    } else if low.contains("cookie") {
        format!(
            "cookies error, re-export cookies.txt ({})",
            first_lines(stderr)
        )
    } else if low.contains("no such option") || low.contains("unknown option") {
        format!("yt-dlp is too old, update it ({})", first_lines(stderr))
    } else {
        first_lines(stderr)
    }
}

fn first_lines(s: &str) -> String {
    s.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .map(|l| l.strip_prefix("ERROR: ").unwrap_or(l))
        .take(2)
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Watch Later and Liked Music answer this when the cookies do not sign
    /// in; the raw yt-dlp line does not tell the user what to do.
    #[test]
    fn missing_playlists_point_at_the_cookies() {
        let err = "ERROR: [youtube:tab] LM: YouTube said: The playlist does not exist.";
        assert!(friendly(err).contains("ytui auth"));
    }
}
