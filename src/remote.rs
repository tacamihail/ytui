//! Client side of the optional remote resolver.
//!
//! The resolver only turns a watch URL into AAC candidates. Probing those
//! candidates and playing the audio stay local: googlevideo URLs can be bound
//! to the IP that requested them, so the machine that plays should be the
//! machine that checks them.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use if_addrs::{get_if_addrs, IfAddr};
use serde::Deserialize;

use crate::config::{self, Config};
use ytui_resolve::{Resolved, UA};

/// Port `ytui-resolver` listens on by default.
pub const SCAN_PORT: u16 = 8787;
/// How long a `?`-menu or install-time scan may take.
pub const SCAN_BUDGET: Duration = Duration::from_secs(10);
/// Per-host connect/read timeout during a scan. Loopback and LAN hosts answer
/// in milliseconds, so a silent one should not stretch the sweep.
const PROBE_TIMEOUT: Duration = Duration::from_millis(600);
/// Timeout for tailnet peers. Reaching one through WSL's NAT and the Tailscale
/// tunnel takes about a second even for a tiny health request.
const TAILNET_PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
/// Parallel probes; a /24 then finishes in about three seconds when every host
/// is silent, which fits the scan budget.
const SCAN_WORKERS: usize = 64;
/// Hard cap on addresses so a strange interface cannot turn a scan into a
/// minute-long crawl.
const MAX_TARGETS: usize = 1024;

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
    /// Whether this peer would need a bearer token for `/v1/resolve`.
    #[serde(default)]
    pub auth: bool,
}

/// A resolver found by [`scan_for`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub url: String,
    pub version: String,
    /// `true` when this peer would need a token (the server is not set to
    /// trust private peers, or no token is configured locally).
    pub auth: bool,
    pub ms: u64,
}

/// `None` when no resolver is configured, so callers keep using local yt-dlp.
pub fn settings(cfg: &Config) -> Option<Settings> {
    let base = env_nonempty("YTUI_RESOLVER").unwrap_or_else(|| cfg.resolver_url.trim().to_string());
    let base = base.trim_end_matches('/').to_string();
    if base.is_empty() {
        return None;
    }
    let token = token_from(cfg);
    Some(Settings {
        base,
        token,
        timeout: Duration::from_secs(cfg.resolver_timeout_secs.max(1)),
        fallback: cfg.resolver_fallback,
    })
}

/// Whether a token is configured, even when no resolver URL is set yet. The
/// scan uses this to decide if a token-protected server is usable.
pub fn has_token(cfg: &Config) -> bool {
    token_from(cfg).is_some()
}

fn token_from(cfg: &Config) -> Option<String> {
    env_nonempty("YTUI_RESOLVER_TOKEN")
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

/// A request agent whose connect timeout matches `timeout`. ureq's default
/// connect timeout is 30 seconds and `.timeout()` does not override it, which
/// turns a probe of a silent host into a 30-second stall.
fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(timeout)
        .timeout(timeout)
        .build()
}

/// Fetches `/v1/health`. Errors are already human-readable for `ytui doctor`.
pub fn health(s: &Settings) -> Result<(Health, u64), String> {
    let started = Instant::now();
    let timeout = s.timeout.min(Duration::from_secs(5));
    let resp = agent(timeout)
        .get(&format!("{}/v1/health", s.base))
        .set("User-Agent", UA)
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
    let mut req = agent(s.timeout).get(&target).set("User-Agent", UA);
    if let Some(token) = &s.token {
        req = req.set("Authorization", &format!("Bearer {token}"));
    }
    let resp = req.call().map_err(http_error)?;
    let body = resp
        .into_string()
        .map_err(|e| format!("cannot read resolver response: {e}"))?;
    serde_json::from_str::<Resolved>(&body).map_err(|e| format!("bad resolver json: {e}"))
}

/// Scans localhost, each private IPv4 `/24` and Tailscale peers on the default
/// resolver port. The caller gets at most `SCAN_BUDGET` of wall-clock time.
/// Results are ordered by "token not needed" first, then by response time.
pub fn scan_for(budget: Duration) -> Vec<Found> {
    scan_port(SCAN_PORT, budget)
}

fn scan_port(port: u16, budget: Duration) -> Vec<Found> {
    let targets = scan_targets();
    if targets.is_empty() {
        return Vec::new();
    }
    let deadline = Instant::now() + budget;
    let next = AtomicUsize::new(0);
    let found = Mutex::new(Vec::new());
    // A tokenless answer is what the caller wants, so stop probing as soon as
    // one arrives instead of sweeping every adapter's /24 for a faster one.
    let tokenless = AtomicBool::new(false);
    let workers = SCAN_WORKERS.min(targets.len()).max(1);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if tokenless.load(Ordering::Relaxed) {
                    break;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(&ip) = targets.get(i) else {
                    break;
                };
                if let Some(hit) = probe(ip, port, probe_timeout(ip).min(left)) {
                    if !hit.auth {
                        tokenless.store(true, Ordering::Relaxed);
                    }
                    found.lock().unwrap_or_else(|e| e.into_inner()).push(hit);
                }
            });
        }
    });

    let mut found = found.into_inner().unwrap_or_else(|e| e.into_inner());
    found.sort_by(|a, b| {
        a.auth
            .cmp(&b.auth)
            .then(a.ms.cmp(&b.ms))
            .then(a.url.cmp(&b.url))
    });
    found
}

fn scan_targets() -> Vec<IpAddr> {
    let mut targets = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    let mut seen = HashSet::new();
    seen.insert(targets[0]);

    let mut push = |ip: IpAddr| {
        if targets.len() < MAX_TARGETS && seen.insert(ip) {
            targets.push(ip);
        }
    };

    // Tailscale peers answer quickly when they exist but are the likeliest
    // remote resolvers, so probe them before the /24 sweep.
    add_tailscale_peers(&mut push);

    if let Ok(interfaces) = get_if_addrs() {
        for iface in interfaces {
            match iface.addr {
                // 127.0.0.1 is already the first target; the rest of the
                // loopback /24 never hosts a resolver.
                IfAddr::V4(v4) if v4.ip.is_loopback() => {}
                IfAddr::V4(v4) if ytui_resolve::is_private_ip(IpAddr::V4(v4.ip)) => {
                    add_v4_range(&mut push, v4.ip, v4.prefixlen);
                }
                IfAddr::V6(v6) if ytui_resolve::is_private_ip(IpAddr::V6(v6.ip)) => {
                    // Scanning an IPv6 prefix is not practical; probe the
                    // machine's own ULA/link-local address only.
                    push(IpAddr::V6(v6.ip));
                }
                _ => {}
            }
        }
    }
    targets
}

/// Adds the hosts around `ip` in the smallest useful prefix: never wider than
/// a `/24`, so a `10.0.0.0/8` interface still costs 254 probes, and a `/32`
/// only probes that address.
fn add_v4_range(push: &mut impl FnMut(IpAddr), ip: Ipv4Addr, prefixlen: u8) {
    let bits = prefixlen.clamp(24, 32);
    let size = 1u32 << (32 - bits);
    let mask = u32::MAX << (32 - bits);
    let base = u32::from(ip) & mask;
    let range = if size <= 4 { 0..size } else { 1..size - 1 };
    for n in range {
        push(IpAddr::V4(Ipv4Addr::from(base + n)));
    }
}

/// Tailscale peers are usually `/32`s outside the interface's /24. Ask the CLI
/// for the list when it is available; ignore anything else.
fn add_tailscale_peers(push: &mut impl FnMut(IpAddr)) {
    let Some(value) = tailscale_status() else {
        return;
    };
    if let Some(node) = value.get("Self") {
        add_tailscale_node(node, push);
    }
    if let Some(peers) = value.get("Peer").and_then(|v| v.as_object()) {
        for node in peers.values() {
            add_tailscale_node(node, push);
        }
    }
}

/// Runs `tailscale status --json`, also trying the Windows executable name so
/// a WSL install can see the host's tailnet, and the macOS app bundle path
/// when the CLI is not on `PATH`. A missing CLI is not an error.
fn tailscale_status() -> Option<serde_json::Value> {
    const COMMANDS: [&str; 3] = [
        // Linux, Windows and the macOS CLI symlink.
        "tailscale",
        // WSL interop: the host's Windows install.
        "tailscale.exe",
        // macOS app bundle.
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ];
    for cmd in COMMANDS {
        let Ok(out) = Command::new(cmd).args(["status", "--json"]).output() else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&out.stdout) {
            return Some(value);
        }
    }
    None
}

fn add_tailscale_node(node: &serde_json::Value, push: &mut impl FnMut(IpAddr)) {
    if !node.get("Online").and_then(|v| v.as_bool()).unwrap_or(true) {
        return;
    }
    let Some(ips) = node.get("TailscaleIPs").and_then(|v| v.as_array()) else {
        return;
    };
    for ip in ips {
        if let Some(addr) = ip.as_str().and_then(|s| s.parse::<IpAddr>().ok()) {
            push(addr);
        }
    }
}

/// Tailscale peers live in `100.64.0.0/10`, outside the machine's own private
/// `/24`, and are the targets normally reached through a tunnel. IPv6 targets
/// are only ever a local ULA/link-local address or a peer's tailnet address.
fn probe_timeout(ip: IpAddr) -> Duration {
    let tailnet = match ip {
        IpAddr::V4(v4) => {
            let range = u32::from(v4) & 0xffc0_0000;
            range == u32::from(Ipv4Addr::new(100, 64, 0, 0))
        }
        IpAddr::V6(_) => true,
    };
    if tailnet {
        TAILNET_PROBE_TIMEOUT
    } else {
        PROBE_TIMEOUT
    }
}

fn probe(ip: IpAddr, port: u16, timeout: Duration) -> Option<Found> {
    let host = match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    let base = format!("http://{host}:{port}");
    let started = Instant::now();
    let resp = agent(timeout)
        .get(&format!("{base}/v1/health"))
        .set("User-Agent", UA)
        .call()
        .ok()?;
    if resp.status() != 200 {
        return None;
    }
    let body = resp.into_string().ok()?;
    let health: Health = serde_json::from_str(&body).ok()?;
    if !health.ok || health.version.is_empty() {
        return None;
    }
    Some(Found {
        url: base,
        version: health.version,
        auth: health.auth,
        ms: started.elapsed().as_millis() as u64,
    })
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

    #[test]
    fn a_24_scans_hosts_but_not_network_or_broadcast() {
        let mut ips = Vec::new();
        add_v4_range(&mut |ip| ips.push(ip), Ipv4Addr::new(192, 168, 1, 42), 24);
        assert_eq!(ips.len(), 254);
        assert!(ips.contains(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1))));
        assert!(!ips.contains(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 0))));
        assert!(!ips.contains(&IpAddr::V4(Ipv4Addr::new(192, 168, 1, 255))));
    }

    #[test]
    fn a_32_scans_only_that_address() {
        let mut ips = Vec::new();
        add_v4_range(&mut |ip| ips.push(ip), Ipv4Addr::new(100, 64, 0, 7), 32);
        assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(100, 64, 0, 7))]);
    }

    #[test]
    fn tailnet_targets_get_the_longer_probe_timeout() {
        let tailnet = IpAddr::V4(Ipv4Addr::new(100, 126, 49, 124));
        assert_eq!(probe_timeout(tailnet), TAILNET_PROBE_TIMEOUT);
        assert_eq!(
            probe_timeout(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5))),
            PROBE_TIMEOUT
        );
        assert_eq!(
            probe_timeout(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)),
            TAILNET_PROBE_TIMEOUT
        );
    }

    #[test]
    fn finds_a_resolver_health_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf);
            let body = r#"{"ok":true,"version":"0.4.0","auth":false}"#;
            let _ = write!(
                sock,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        });
        let found = probe(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(found.version, "0.4.0");
        assert!(!found.auth);
        handle.join().unwrap();
    }
}
