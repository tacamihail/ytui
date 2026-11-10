//! yt-dlp invocation and AAC candidate ranking.
//!
//! This is the part of resolution that both the TUI and the optional remote
//! resolver need: run `yt-dlp -j`, pull out the metadata, and order the AAC
//! formats best-first. Probing a candidate URL stays with the player because
//! googlevideo URLs can be bound to the requesting IP.

use std::io::{self, Read};
use std::net::{IpAddr, Ipv4Addr};
use std::process::{Command, Output, Stdio};
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

/// Runs `cmd` and collects its output, killing it at `timeout`.
///
/// Both pipes are drained on their own threads so a large JSON answer cannot
/// fill a pipe and make the process look hung.
fn output_with_timeout(cmd: &mut Command, timeout: Duration) -> io::Result<Output> {
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
    let mut c = command();
    c.args(["--no-warnings", "-j", url]);
    if let Some((flag, value)) = cookies {
        c.arg(flag).arg(value);
    }
    let out = output_with_timeout(&mut c, RESOLVE_TIMEOUT).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => "yt-dlp not found, install it and run: ytui doctor".to_string(),
        io::ErrorKind::TimedOut => "yt-dlp timed out while resolving the stream".to_string(),
        _ => format!("yt-dlp: {e}"),
    })?;
    if !out.status.success() {
        return Err(friendly(&String::from_utf8_lossy(&out.stderr)));
    }
    parse_info(&out.stdout)
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
