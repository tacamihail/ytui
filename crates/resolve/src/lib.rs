//! yt-dlp invocation and AAC candidate ranking.
//!
//! This is the part of resolution that both the TUI and the optional remote
//! resolver need: run `yt-dlp -j`, pull out the metadata, and order the AAC
//! formats best-first. Probing a candidate URL stays with the player because
//! googlevideo URLs can be bound to the requesting IP.

use std::fs::OpenOptions;
use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Browser-ish UA; googlevideo accepts any modern string.
pub const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                      Chrome/126.0.0.0 Safari/537.36";

/// True for addresses that only make sense on the local machine or a private
/// network: loopback, RFC1918, link-local, Tailscale/CGNAT (`100.64.0.0/10`)
/// and IPv6 ULA/link-local. The resolver uses this to decide whether a peer
/// may skip the bearer token, and the client uses it to pick scan targets.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => private_v4(v4),
        IpAddr::V6(v6) => {
            let s = v6.segments();
            v6.is_loopback()
                || (s[0] & 0xfe00) == 0xfc00 // fc00::/7, unique local
                || (s[0] & 0xffc0) == 0xfe80 // fe80::/10, link-local
                || v6.to_ipv4_mapped().is_some_and(private_v4)
        }
    }
}

fn private_v4(v4: Ipv4Addr) -> bool {
    // 100.64.0.0/10 is where Tailscale and carrier NAT put peers; it is not
    // `Ipv4Addr::is_private`, so it needs its own check.
    let o = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || (o[0] == 100 && (64..=127).contains(&o[1]))
}

/// One playable audio URL, ordered by [`Resolved::candidates`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub url: String,
    pub itag: String,
    pub abr: u64,
}

/// Everything a resolve returns: metadata plus the AAC URLs to try.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolved {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub duration: f64,
    pub candidates: Vec<Candidate>,
    /// Set by the resolver when the answer came from its cache. The player
    /// only uses it to mark a resolve in the footer, so older servers that
    /// omit it are fine.
    #[serde(default)]
    pub cached: bool,
}

/// `yt-dlp` plus the directories the installer uses, so a binary installed by
/// `install.sh` is found even when the player was started from a shell that
/// has not picked up the new PATH yet. The same PATH also lets yt-dlp find
/// `deno`, which it needs to solve YouTube's JS challenges.
pub fn tool_path() -> String {
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    // Windows exports USERPROFILE; HOME there may be a MSYS path that native
    // programs cannot use, so it comes second.
    #[cfg(windows)]
    let vars: &[&str] = &["USERPROFILE", "HOME"];
    #[cfg(not(windows))]
    let vars: &[&str] = &["HOME"];

    for var in vars {
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

pub fn command() -> Command {
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
    // yt-dlp re-encodes its console output with the ANSI code page, so on a
    // Windows machine a title like "Arun’s Roof" reaches a pipe as the single
    // byte 0x92 instead of UTF-8. Anything reading a line at a time then sees
    // an invalid stream. Ask for UTF-8 explicitly; the flag is old enough to
    // exist in every yt-dlp that supports the rest of the arguments we pass.
    c.args(["--encoding", "utf-8"]);
    c
}

/// How long a cookie-using run waits for another one before going ahead
/// without the lock. A stuck yt-dlp must not block playback forever; before
/// the lock existed every run was unserialized anyway.
pub const COOKIE_LOCK_TIMEOUT: Duration = Duration::from_secs(45);

/// An exclusive lock on the file passed to `yt-dlp --cookies`, held for one
/// run.
///
/// yt-dlp rewrites that file when it exits, which is also how the rotating
/// `__Secure-1PSIDTS` / `__Secure-3PSIDTS` cookies stay current. Two runs that
/// load the same jar at the same time race on save: the later one can put a
/// stale token back, and YouTube then treats the session as stolen and signs
/// the account out. Serialize the runs instead, across threads and across ytui
/// processes (the player, a second window, `ytui doctor`, `ytui auth`).
pub struct CookieLock {
    _file: std::fs::File,
}

impl CookieLock {
    /// Blocks until the lock is free, up to `wait`.
    pub fn acquire(path: &str, wait: Duration) -> Option<Self> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(format!("{path}.lock"))
            .ok()?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => return Some(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if Instant::now() >= deadline {
                        return None;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => return None,
            }
        }
    }
}

/// Takes the cookie lock when `cookies` is a `--cookies <file>` pair.
///
/// `--cookies-from-browser` needs no lock: yt-dlp reads the browser database
/// and never writes it back.
pub fn lock_cookies(cookies: Option<(&str, &str)>) -> Option<CookieLock> {
    match cookies {
        Some(("--cookies", path)) => CookieLock::acquire(path, COOKIE_LOCK_TIMEOUT),
        _ => None,
    }
}

/// Like [`lock_cookies`], but a jar that stays busy is an error instead of a
/// reason to run anyway.
///
/// Proceeding without the lock is worse than waiting: the run loaded the jar
/// before the lock holder saved its rotation, so its own save at exit puts that
/// older token back. Callers that pass `--cookies` must not start without it.
pub fn lock_cookies_for_run(cookies: Option<(&str, &str)>) -> Result<Option<CookieLock>, String> {
    match cookies {
        Some(("--cookies", path)) => CookieLock::acquire(path, COOKIE_LOCK_TIMEOUT)
            .map(Some)
            .ok_or_else(|| {
                format!("another yt-dlp is still writing {path}; try again in a moment")
            }),
        _ => Ok(None),
    }
}

/// True when a Netscape jar still has a first-party YouTube session cookie.
///
/// YouTube does not empty the jar when it signs a session out: it clears `SID`,
/// `HSID`, `SSID`, the `__Secure-1*` family and their `APISID`/`SAPISID`
/// partners, while visitor cookies and the third-party `__Secure-3*` copies
/// stay. A signed-out jar therefore still looks populated, so callers must look
/// for these names rather than for the file being non-empty.
pub fn has_session_cookies(txt: &str) -> bool {
    const FIRST_PARTY: &[&str] = &[
        "SID",
        "HSID",
        "SSID",
        "APISID",
        "SAPISID",
        "__Secure-1PSID",
        "__Secure-1PAPISID",
    ];
    txt.lines().any(|line| {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') {
            return false;
        }
        matches!(line.split('\t').nth(5), Some(name) if FIRST_PARTY.contains(&name))
    })
}

fn on_path(name: &str) -> bool {
    // On Windows the file on PATH is `name.exe`; a literal `name` never exists.
    let exe = format!("{name}{}", std::env::consts::EXE_SUFFIX);
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|d| d.join(name).is_file() || d.join(&exe).is_file())
        })
        .unwrap_or(false)
}

/// How long a `yt-dlp -j` resolve may take before it is killed.
///
/// A normal answer takes a few seconds (the JS challenge is what costs most of
/// it). Anything longer is a stuck challenge or network call: waiting forever
/// would also freeze the client's background prefetch queue and occupy a
/// resolver slot on the server.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);

/// What a caller sees when its own cancel flag ended the run. The player drops
/// these results silently: a newer selection already took over.
pub const CANCELLED: &str = "resolve cancelled";

/// Runs `cmd` and collects its output, killing it at `timeout` or as soon as
/// `cancel` is raised.
///
/// Both pipes are drained on their own threads so a large JSON answer cannot
/// fill a pipe and make the process look hung. The flag is checked on the same
/// 40 ms tick as the timeout, so a superseded resolve stops burning CPU
/// (yt-dlp plus deno is ~135 MiB) almost immediately.
fn output_with_timeout(
    cmd: &mut Command,
    timeout: Duration,
    cancel: Option<&AtomicBool>,
) -> io::Result<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let mut stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let out = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });
    let err = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out.join();
                let _ = err.join();
                return Err(io::Error::new(io::ErrorKind::Interrupted, CANCELLED));
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(40)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out.join();
                let _ = err.join();
                return Err(io::Error::new(io::ErrorKind::TimedOut, "yt-dlp timed out"));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out.join();
                let _ = err.join();
                return Err(e);
            }
        }
    };

    Ok(Output {
        status,
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    })
}

/// Resolves a watch URL to metadata and a ranked list of AAC candidates.
///
/// YouTube is inconsistent about which AAC bitrates it serves: `itag=140`
/// (128 kbps) works for most videos but returns 403 for others, while
/// `itag=139` (HE-AAC, 48 kbps) almost always works. Callers probe the list
/// in order instead of trusting the first URL `-g` prints.
pub fn resolve(url: &str, cookies: Option<(&str, &str)>) -> Result<Resolved, String> {
    resolve_cancellable(url, cookies, None)
}

/// Like [`resolve`], but kills the yt-dlp child as soon as `cancel` is raised
/// and returns [`CANCELLED`]. Used by the player's foreground load, where a
/// newer selection must not wait for the abandoned track.
pub fn resolve_cancellable(
    url: &str,
    cookies: Option<(&str, &str)>,
    cancel: Option<&AtomicBool>,
) -> Result<Resolved, String> {
    // One writer at a time keeps the rotating session cookies in the jar
    // current; see [`CookieLock`].
    let _lock = lock_cookies_for_run(cookies)?;
    let had_session = jar_session_state(cookies);
    let mut c = command();
    c.args(["--no-warnings", "-j", url]);
    if let Some((flag, value)) = cookies {
        c.arg(flag).arg(value);
    }
    let out = output_with_timeout(&mut c, RESOLVE_TIMEOUT, cancel).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => "yt-dlp not found, install it and run: ytui doctor".to_string(),
        io::ErrorKind::TimedOut => "yt-dlp timed out while resolving the stream".to_string(),
        io::ErrorKind::Interrupted => CANCELLED.to_string(),
        _ => format!("yt-dlp: {e}"),
    })?;
    if had_session == Some(true) && jar_session_state(cookies) == Some(false) {
        return Err(SIGNED_OUT.into());
    }
    if !out.status.success() {
        return Err(friendly(&String::from_utf8_lossy(&out.stderr)));
    }
    parse_info(&out.stdout)
}

/// What YouTube does when it rejects a session that is still being replayed:
/// the first-party cookies are cleared, so the jar looks populated but the
/// account is gone.
pub const SIGNED_OUT: &str = "youtube signed this session out (the first-party cookies were cleared). \
Another copy of the same export is the usual cause: a second ytui (WSL counts), a browser tab \
still using that session, or a resolver started with --cookies. Re-export once and use it on one \
client only, then run `ytui auth <cookies.txt>`.";

/// `Some(true)` when a file-backed jar still signs in, `Some(false)` when it is
/// signed out, `None` for browser cookies or an unreadable file.
fn jar_session_state(cookies: Option<(&str, &str)>) -> Option<bool> {
    match cookies {
        Some(("--cookies", path)) => std::fs::read_to_string(path)
            .ok()
            .map(|txt| has_session_cookies(&txt)),
        _ => None,
    }
}

/// Parses the JSON printed by `yt-dlp -j`. Split out so the ranking can be
/// tested without running yt-dlp.
fn parse_info(json: &[u8]) -> Result<Resolved, String> {
    let info: serde_json::Value =
        serde_json::from_slice(json).map_err(|e| format!("bad yt-dlp json: {e}"))?;
    resolved_from_json(&info)
}

fn resolved_from_json(info: &serde_json::Value) -> Result<Resolved, String> {
    let text = |key: &str| {
        info.get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let channel = {
        let channel = text("channel");
        if channel.is_empty() {
            text("uploader")
        } else {
            channel
        }
    };

    let formats = info
        .get("formats")
        .and_then(|f| f.as_array())
        .ok_or_else(|| "yt-dlp returned no formats".to_string())?;

    let mut ranked: Vec<(u8, u64, Candidate)> = Vec::new();
    for f in formats {
        let Some(url) = f.get("url").and_then(|v| v.as_str()) else {
            continue;
        };
        let ext = f.get("ext").and_then(|v| v.as_str()).unwrap_or("");
        let codec = f.get("acodec").and_then(|v| v.as_str()).unwrap_or("");
        if ext != "m4a" && !codec.starts_with("mp4a") {
            continue;
        }
        let itag = f
            .get("format_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // Lower rank = preferred. 141 is 256 kbps (Music Premium), 140 is
        // 128 kbps, 139 is 48 kbps HE-AAC.
        let rank = match itag.as_str() {
            "141" => 0,
            "140" => 1,
            "139" => 2,
            _ => 3,
        };
        let abr = f.get("abr").and_then(|v| v.as_f64()).unwrap_or(0.0) as u64;
        ranked.push((
            rank,
            abr,
            Candidate {
                url: url.to_string(),
                itag,
                abr,
            },
        ));
    }
    if ranked.is_empty() {
        return Err("no AAC stream for this track (opus only)".into());
    }
    ranked.sort_by_key(|(rank, abr, _)| (*rank, std::cmp::Reverse(*abr)));

    Ok(Resolved {
        id: text("id"),
        title: text("title"),
        channel,
        duration: info.get("duration").and_then(|v| v.as_f64()).unwrap_or(0.0),
        candidates: ranked.into_iter().map(|(_, _, c)| c).collect(),
        cached: false,
    })
}

/// Maps yt-dlp's stderr to something a listener can act on.
pub fn friendly(stderr: &str) -> String {
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

    /// A superseded resolve must stop the child instead of letting it run to
    /// completion; on a Pi that is a whole yt-dlp + deno pair.
    #[test]
    fn a_cancelled_run_kills_the_child_early() {
        let mut cmd = if cfg!(windows) {
            // Direct child: `cmd /C ping` would leave ping holding the pipes
            // after cmd is killed, and the drain threads would wait it out.
            let mut c = Command::new("ping");
            c.args(["-n", "10", "127.0.0.1"]);
            c
        } else {
            let mut c = Command::new("sleep");
            c.arg("10");
            c
        };
        let cancel = AtomicBool::new(false);
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(150));
                cancel.store(true, Ordering::Relaxed);
            });
            let err = output_with_timeout(&mut cmd, Duration::from_secs(20), Some(&cancel))
                .expect_err("a cancelled run does not return output");
            assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        });
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the child should die on the next poll, not at the timeout"
        );
    }

    /// The whole point: two yt-dlp runs must not rewrite the same jar at the
    /// same time, or one can save a stale `__Secure-1PSIDTS` over the other.
    #[test]
    fn cookie_runs_are_serialized() {
        let dir = std::env::temp_dir().join(format!("ytui-cookielock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let jar = dir.join("cookies.txt");
        std::fs::write(&jar, "x").unwrap();
        let path = jar.to_str().unwrap();

        let held = CookieLock::acquire(path, Duration::from_millis(100)).expect("first lock");
        assert!(CookieLock::acquire(path, Duration::from_millis(100)).is_none());
        drop(held);
        assert!(CookieLock::acquire(path, Duration::from_millis(100)).is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_cookies_need_no_lock() {
        assert!(lock_cookies(Some(("--cookies-from-browser", "chrome"))).is_none());
        assert!(lock_cookies(None).is_none());
        assert!(lock_cookies_for_run(Some(("--cookies-from-browser", "chrome"))).is_ok());
        assert!(lock_cookies_for_run(None).unwrap().is_none());
    }

    /// YouTube empties the first-party cookies on sign-out but leaves visitor
    /// and `__Secure-3*` rows, which is what made the old "does it mention
    /// SID?" check pass on a dead jar.
    #[test]
    fn a_signed_out_jar_has_no_session_cookies() {
        let signed_in = "\
# Netscape HTTP Cookie File
.youtube.com\tTRUE\t/\tTRUE\t1900000000\t__Secure-1PSID\tabc
#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1900000000\tSID\tdef
";
        assert!(has_session_cookies(signed_in));
        let signed_out = "\
# Netscape HTTP Cookie File
.youtube.com\tTRUE\t/\tTRUE\t1900000000\t__Secure-3PSID\tabc
.youtube.com\tTRUE\t/\tTRUE\t1900000000\t__Secure-1PSIDTS\tabc
.youtube.com\tTRUE\t/\tTRUE\t1900000000\tVISITOR_INFO1_LIVE\tabc
";
        assert!(!has_session_cookies(signed_out));
    }

    fn format(id: &str, ext: &str, codec: &str, abr: f64) -> serde_json::Value {
        serde_json::json!({
            "url": format!("https://example.invalid/{id}"),
            "format_id": id,
            "ext": ext,
            "acodec": codec,
            "abr": abr,
        })
    }

    #[test]
    fn ranks_music_then_normal_then_he_aac_by_bitrate() {
        let info = serde_json::json!({
            "id": "dQw4w9WgXcQ",
            "title": "Song",
            "channel": "Artist",
            "duration": 213.0,
            "formats": [
                format("139", "m4a", "mp4a.40.5", 48.0),
                format("140", "m4a", "mp4a.40.2", 128.0),
                format("141", "m4a", "mp4a.40.2", 256.0),
                format("251", "webm", "opus", 160.0),
                format("140", "m4a", "mp4a.40.2", 130.0),
            ],
        });
        let r = resolved_from_json(&info).unwrap();
        let ids: Vec<_> = r.candidates.iter().map(|c| c.itag.as_str()).collect();
        assert_eq!(ids, ["141", "140", "140", "139"]);
        assert_eq!(r.candidates[1].abr, 130);
        assert_eq!(r.channel, "Artist");
        assert_eq!(r.duration, 213.0);
    }

    #[test]
    fn falls_back_to_uploader_when_channel_is_empty() {
        let info = serde_json::json!({
            "uploader": "Uploader",
            "formats": [format("140", "m4a", "mp4a.40.2", 128.0)],
        });
        assert_eq!(resolved_from_json(&info).unwrap().channel, "Uploader");
    }

    #[test]
    fn rejects_opus_only() {
        let info = serde_json::json!({
            "formats": [format("251", "webm", "opus", 160.0)],
        });
        assert_eq!(
            resolved_from_json(&info).unwrap_err(),
            "no AAC stream for this track (opus only)"
        );
    }

    #[test]
    fn recognises_private_and_cgnat_addresses() {
        for ip in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.5.4",
            "192.168.1.20",
            "169.254.3.4",
            "100.64.0.5",
            "100.127.255.254",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:192.168.1.9",
        ] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["8.8.8.8", "100.63.0.1", "100.128.0.1", "2606:4700::1111"] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn maps_common_ytdlp_errors() {
        assert_eq!(
            friendly("ERROR: [youtube] x: Video unavailable"),
            "unavailable on youtube"
        );
        assert_eq!(
            friendly("ERROR: Sign in to confirm you're not a bot"),
            "youtube wants a sign-in check, try again later"
        );
        assert_eq!(
            friendly("ERROR: Private video. Sign in if you've been granted access"),
            "private: sign-in required, run `ytui auth <cookies.txt>`"
        );
    }

    /// Watch Later and Liked Music answer this when the cookies do not sign
    /// in; the raw yt-dlp line does not tell the user what to do.
    #[test]
    fn missing_playlists_point_at_the_cookies() {
        let err = "ERROR: [youtube:tab] LM: YouTube said: The playlist does not exist.";
        assert!(friendly(err).contains("ytui auth"));
    }

    /// A stuck resolve must not pin a prefetch queue or a server slot forever.
    #[cfg(unix)]
    #[test]
    fn a_hung_resolver_is_killed() {
        let mut c = Command::new("sleep");
        c.arg("5");
        let start = Instant::now();
        let err = output_with_timeout(&mut c, Duration::from_millis(200)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn output_is_collected_while_the_process_runs() {
        let mut c = Command::new("sh");
        c.args(["-c", "printf hello"]);
        let out = output_with_timeout(&mut c, Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, b"hello");
    }
}
