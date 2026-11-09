//! Client side of the optional remote resolver.
//!
//! The resolver only turns a watch URL into AAC candidates. Probing those
//! candidates and playing the audio stay local: googlevideo URLs can be bound
//! to the IP that requested them, so the machine that plays should be the
//! machine that checks them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::config::{self, Config};
use ytui_resolve::{Resolved, UA};

/// Resolver settings with the env overrides already applied.
pub struct Settings {
    pub base: String,
    pub token: Option<String>,
    pub timeout: Duration,
    pub fallback: bool,
}

impl std::fmt::Debug for Settings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Settings")
            .field("base", &self.base)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("timeout", &self.timeout)
            .field("fallback", &self.fallback)
            .finish()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Health {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub ytdlp: Option<String>,
    #[serde(default)]
    pub deno: Option<String>,
}

/// `None` when no resolver is configured, so callers keep using local yt-dlp.
pub fn settings(cfg: &Config) -> Option<Settings> {
    let base = env_nonempty("YTUI_RESOLVER").unwrap_or_else(|| cfg.resolver_url.trim().to_string());
    let base = base.trim_end_matches('/').to_string();
    if base.is_empty() {
        return None;
    }
    let token = env_nonempty("YTUI_RESOLVER_TOKEN")
        .or_else(|| {
            let t = cfg.resolver_token.trim();
            (!t.is_empty()).then(|| t.to_string())
        })
        .or_else(|| {
            let explicit = cfg.resolver_token_file.trim();
            let path = if explicit.is_empty() {
                let default = config::config_dir().join("resolver.token");
                default.exists().then_some(default)
            } else {
                Some(expand_home(explicit))
            };
            path.and_then(|p| std::fs::read_to_string(p).ok())
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())
        });
    Some(Settings {
        base,
        token,
        timeout: Duration::from_secs(cfg.resolver_timeout_secs.max(1)),
        fallback: cfg.resolver_fallback,
    })
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// Fetches `/v1/health`. Errors are already human-readable for `ytui doctor`.
pub fn health(s: &Settings) -> Result<(Health, u64), String> {
    let started = Instant::now();
    let resp = ureq::get(&format!("{}/v1/health", s.base))
        .set("User-Agent", UA)
        .timeout(s.timeout.min(Duration::from_secs(5)))
        .call()
        .map_err(http_error)?;
    let ms = started.elapsed().as_millis() as u64;
    let body = resp
        .into_string()
        .map_err(|e| format!("cannot read health: {e}"))?;
    let health =
        serde_json::from_str::<Health>(&body).map_err(|e| format!("bad health json: {e}"))?;
    Ok((health, ms))
}

/// Asks the resolver for candidates. `refresh=1` bypasses its cache; the
/// client uses that once when every cached URL was refused.
pub fn resolve(s: &Settings, url: &str, refresh: bool) -> Result<Resolved, String> {
    let mut target = format!("{}/v1/resolve?url={}", s.base, urlencode(url));
    if refresh {
        target.push_str("&refresh=1");
    }
    let mut req = ureq::get(&target).set("User-Agent", UA).timeout(s.timeout);
    if let Some(token) = &s.token {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    let resp = req.call().map_err(http_error)?;
    let body = resp
        .into_string()
        .map_err(|e| format!("cannot read resolver response: {e}"))?;
    serde_json::from_str::<Resolved>(&body).map_err(|e| format!("bad resolver json: {e}"))
}

fn http_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            status_message(code, &body)
        }
        ureq::Error::Transport(t) => {
            // ureq formats this as "<url>: <kind>: <detail>". The URL and the
            // operating-system detail are noise in a one-line status message.
            let text = t.to_string();
            let kind = text.split(": ").nth(1).unwrap_or(text.as_str()).trim();
            format!("unreachable ({kind})")
        }
    }
}

fn status_message(code: u16, body: &str) -> String {
    #[derive(Deserialize)]
    struct ErrorBody {
        #[serde(default)]
        error: String,
    }
    let detail = serde_json::from_str::<ErrorBody>(body)
        .map(|e| e.error)
        .unwrap_or_else(|_| body.trim().to_string());
    let detail = detail.trim();
    match code {
        401 => "bad token (check resolver_token / resolver_token_file)".into(),
        429 => "rate limited by resolver".into(),
        502 if !detail.is_empty() => detail.to_string(),
        400..=499 if !detail.is_empty() => format!("http {code}: {detail}"),
        _ if !detail.is_empty() => format!("http {code}: {detail}"),
        _ => format!("http {code}"),
    }
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------- fallback notice ----------------

static FALLBACK_NOTIFIED: AtomicBool = AtomicBool::new(false);
static FALLBACK_NOTE: Mutex<Option<String>> = Mutex::new(None);

/// Records that the remote resolver failed and local yt-dlp took over. Only
/// the first failure of a session reaches the UI, so a dead server does not
/// produce a toast on every track.
pub fn note_fallback(reason: &str) {
    if FALLBACK_NOTIFIED.swap(true, Ordering::SeqCst) {
        return;
    }
    let reason = short_reason(reason);
    *FALLBACK_NOTE.lock().unwrap_or_else(|e| e.into_inner()) = Some(format!(
        "resolver unavailable ({reason}), using local yt-dlp"
    ));
}

pub fn take_note() -> Option<String> {
    FALLBACK_NOTE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

fn short_reason(s: &str) -> String {
    let line = s.lines().next().unwrap_or(s).trim();
    if line.chars().count() > 90 {
        let mut short: String = line.chars().take(87).collect();
        short.push_str("...");
        short
    } else if line.is_empty() {
        "no response".into()
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    fn one_shot(status: &str, body: &str) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let status = status.to_string();
        let body = body.to_string();
        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(response.as_bytes()).unwrap();
            request
        });
        (format!("http://{addr}"), handle)
    }

    fn settings(base: &str) -> Settings {
        Settings {
            base: base.into(),
            token: Some("secret".into()),
            timeout: Duration::from_secs(2),
            fallback: true,
        }
    }

    fn sample() -> String {
        r#"{"v":1,"id":"dQw4w9WgXcQ","title":"Song","channel":"Artist","duration":213.0,"ms":12,"candidates":[{"url":"https://example.invalid/140","itag":"140","abr":128}]}"#.into()
    }

    #[test]
    fn parses_a_resolve_and_sends_the_token() {
        let (base, server) = one_shot("200 OK", &sample());
        let got = resolve(
            &settings(&base),
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            false,
        )
        .unwrap();
        assert_eq!(got.title, "Song");
        assert_eq!(got.candidates[0].itag, "140");
        let request = server.join().unwrap();
        assert!(request.starts_with(
            "GET /v1/resolve?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3DdQw4w9WgXcQ "
        ));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: bearer secret"));
    }

    #[test]
    fn refresh_adds_the_query_flag() {
        let (base, server) = one_shot("200 OK", &sample());
        let _ = resolve(&settings(&base), "https://youtu.be/dQw4w9WgXcQ", true).unwrap();
        let request = server.join().unwrap();
        assert!(request.contains("&refresh=1 "), "{request}");
    }

    #[test]
    fn maps_bad_token() {
        let (base, server) = one_shot("401 Unauthorized", r#"{"error":"bad token"}"#);
        let err = resolve(&settings(&base), "https://youtu.be/dQw4w9WgXcQ", false).unwrap_err();
        assert!(err.contains("bad token"), "{err}");
        server.join().unwrap();
    }

    #[test]
    fn maps_resolver_failure_text() {
        let (base, server) = one_shot("502 Bad Gateway", r#"{"error":"unavailable on youtube"}"#);
        let err = resolve(&settings(&base), "https://youtu.be/dQw4w9WgXcQ", false).unwrap_err();
        assert_eq!(err, "unavailable on youtube");
        server.join().unwrap();
    }

    #[test]
    fn maps_unreachable() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let err = resolve(
            &settings(&format!("http://{addr}")),
            "https://youtu.be/x",
            false,
        )
        .unwrap_err();
        assert!(err.contains("unreachable"), "{err}");
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(
            urlencode("https://www.youtube.com/watch?v=dQw4w9WgXcQ&x=1"),
            "https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3DdQw4w9WgXcQ%26x%3D1"
        );
    }
}
