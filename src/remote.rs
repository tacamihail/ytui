//! Client side of the optional remote resolver.
//!
//! The resolver only turns a watch URL into AAC candidates. Probing those
//! candidates and playing the audio stay local: googlevideo URLs can be bound
//! to the IP that requested them, so the machine that plays should be the
//! machine that checks them.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use if_addrs::{get_if_addrs, IfAddr};
use serde::Deserialize;

use crate::config::{self, Config};
use ytui_resolve::{is_private_ip, Resolved, UA};

/// Port `ytui-resolver` listens on by default.
pub const SCAN_PORT: u16 = 8787;
/// How long a `?`-menu or install-time scan may take.
pub const SCAN_BUDGET: Duration = Duration::from_secs(10);
/// The interactive scan may take longer: a person is watching it, while an
/// unattended `install.sh` should not sit there for a minute.
pub const MENU_SCAN_BUDGET: Duration = Duration::from_secs(60);
/// After the first tokenless answer, keep probing for this long so a second,
/// faster resolver on the same network still gets a chance to answer.
const SCAN_GRACE: Duration = Duration::from_millis(1500);
/// Per-host connect/read timeout during a scan. A resolver on the local
/// network answers in milliseconds; a silent host should not hold the sweep.
/// Kept generous because `/v1/health` on an older server can take about a
/// second while it shells out to `yt-dlp --version` and `deno --version`.
const PROBE_TIMEOUT: Duration = Duration::from_millis(2500);
/// Overlay peers answer through a tunnel, and their `/v1/health` can be the
/// first one that shells out for tool versions, so give them a little more
/// room than a LAN host while still respecting the overall scan deadline.
const OVERLAY_PROBE_TIMEOUT: Duration = Duration::from_millis(3000);
/// A wedged `tailscale` CLI must not hold the scan: it gets this long to
/// print `status --json` before it is killed and the sweep continues.
const OVERLAY_CLI_TIMEOUT: Duration = Duration::from_secs(2);
/// Parallel probes; a /24 then finishes in about five seconds when every host
/// is silent, which fits the scan budget.
const SCAN_WORKERS: usize = 128;
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
    /// Whether the resolver passes a cookie file of its own to yt-dlp. The
    /// player's cookies are never sent to it.
    #[serde(default)]
    pub cookies: bool,
    /// Median cold-resolve time the server has seen, when it has resolved
    /// anything since it started.
    #[serde(default)]
    pub resolve_ms: Option<u64>,
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
    /// The server's own median resolve time, when it reported one.
    pub solve_ms: Option<u64>,
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

/// Settings for a resolver URL given on the command line (`ytui discover
/// --url`), which ignores `$YTUI_RESOLVER` but still uses the configured token.
pub fn settings_at(base: &str, cfg: &Config) -> Settings {
    Settings {
        base: base.trim().trim_end_matches('/').to_string(),
        token: token_from(cfg),
        timeout: Duration::from_secs(cfg.resolver_timeout_secs.max(1)),
        fallback: cfg.resolver_fallback,
    }
}

/// Whether a token is configured, even when no resolver URL is set yet. The
/// scan uses this to decide if a token-protected server is usable.
pub fn has_token(cfg: &Config) -> bool {
    token_from(cfg).is_some()
}

/// Where the configured resolver lives, shown in the help overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reach {
    Local,
    Lan,
    Public,
}

impl Reach {
    pub fn label(self) -> &'static str {
        match self {
            Reach::Local => "LOCAL",
            Reach::Lan => "LAN",
            Reach::Public => "PUBLIC",
        }
    }
}

/// Classifies a resolver URL by its host: loopback is local, private/tailnet
/// addresses and mDNS-style names are LAN, everything else is public.
pub fn reach(url: &str) -> Reach {
    let Some(host) = host_of(url) else {
        return Reach::Public;
    };
    if host.eq_ignore_ascii_case("localhost") {
        return Reach::Local;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        return if ip.is_loopback() {
            Reach::Local
        } else if is_private_ip(ip) {
            Reach::Lan
        } else {
            Reach::Public
        };
    }
    let host = host.to_ascii_lowercase();
    let local_name = [".local", ".lan", ".home", ".internal", ".localdomain"]
        .iter()
        .any(|suffix| host.ends_with(suffix));
    if local_name {
        Reach::Lan
    } else {
        Reach::Public
    }
}

/// Host part of a URL without scheme, userinfo, port or IPv6 brackets.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?;
    if let Some(v6) = host.strip_prefix('[') {
        return v6.split(']').next().map(str::to_string);
    }
    let host = host.split(':').next()?;
    (!host.is_empty()).then(|| host.to_string())
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
                Some(config::expand_tilde(explicit))
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

/// Scans localhost and each private IPv4 `/24` on the default resolver port.
/// The caller gets at most `budget` of wall-clock time. Results are ordered by
/// "token not needed" first, then by the estimated wait per track.
pub fn scan_for(budget: Duration) -> Vec<Found> {
    scan_for_cancellable(budget, None)
}

/// Like [`scan_for`], but stops early when `cancel` becomes true. The help
/// overlay uses it so closing the scan does not leave a minute-long sweep
/// running in the background.
pub fn scan_for_cancellable(budget: Duration, cancel: Option<&AtomicBool>) -> Vec<Found> {
    scan_port(SCAN_PORT, budget, cancel)
}

fn scan_port(port: u16, budget: Duration, cancel: Option<&AtomicBool>) -> Vec<Found> {
    let targets = scan_targets();
    if targets.is_empty() {
        return Vec::new();
    }
    let deadline = Instant::now() + budget;
    let next = AtomicUsize::new(0);
    let found = Mutex::new(Vec::new());
    // After the first tokenless answer, keep probing for a moment so other
    // resolvers on the same network can answer too and the fastest wins.
    let first_tokenless: Mutex<Option<Instant>> = Mutex::new(None);
    let workers = SCAN_WORKERS.min(targets.len()).max(1);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    break;
                }
                if let Some(at) = *first_tokenless.lock().unwrap_or_else(|e| e.into_inner()) {
                    if at.elapsed() >= SCAN_GRACE {
                        break;
                    }
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
                        let mut slot = first_tokenless.lock().unwrap_or_else(|e| e.into_inner());
                        if slot.is_none() {
                            *slot = Some(Instant::now());
                        }
                    }
                    found.lock().unwrap_or_else(|e| e.into_inner()).push(hit);
                }
            });
        }
    });

    let mut found = found.into_inner().unwrap_or_else(|e| e.into_inner());
    found.sort_by(rank);
    found
}

/// Tokenless first, then the shortest estimated wait per track.
fn rank(a: &Found, b: &Found) -> std::cmp::Ordering {
    a.auth
        .cmp(&b.auth)
        .then(score(a).cmp(&score(b)))
        .then(a.url.cmp(&b.url))
}

/// The wait a client would see for a track: the server's own median resolve
/// time plus the health round-trip. A server that has not resolved yet is
/// judged on the round-trip alone.
fn score(found: &Found) -> u64 {
    found.ms + found.solve_ms.unwrap_or(0)
}

fn scan_targets() -> Vec<IpAddr> {
    let mut targets = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    let mut seen = HashSet::new();
    seen.insert(targets[0]);

    // The real LAN goes before virtual switches: Windows machines often carry
    // VMware, Hyper-V, WSL and VirtualBox adapters, each with its own /24, and
    // they used to push the physical network out of the scan.
    let mut overlay: Vec<IpAddr> = Vec::new();
    let mut primary: Vec<IpAddr> = Vec::new();
    let mut virtual_switches: Vec<IpAddr> = Vec::new();

    // Overlay peers are /32 routes outside every local /24, so the interface
    // sweep below can never reach them. Probe them first: a tailnet resolver
    // is the common case for a laptop that is not at home.
    add_overlay_peers(&mut |ip| overlay.push(ip));

    if let Ok(interfaces) = get_if_addrs() {
        for iface in interfaces {
            match iface.addr {
                // 127.0.0.1 is already the first target; the rest of the
                // loopback /24 never hosts a resolver.
                IfAddr::V4(v4) if v4.ip.is_loopback() => {}
                IfAddr::V4(v4) if ytui_resolve::is_private_ip(IpAddr::V4(v4.ip)) => {
                    let out = if host_only_switch(v4.ip) {
                        &mut virtual_switches
                    } else {
                        &mut primary
                    };
                    add_v4_range(&mut |ip| out.push(ip), v4.ip, v4.prefixlen);
                }
                IfAddr::V6(v6) if ytui_resolve::is_private_ip(IpAddr::V6(v6.ip)) => {
                    // Scanning an IPv6 prefix is not practical; probe the
                    // machine's own ULA/link-local address only.
                    primary.push(IpAddr::V6(v6.ip));
                }
                _ => {}
            }
        }
    }

    for ip in overlay.into_iter().chain(primary).chain(virtual_switches) {
        if targets.len() >= MAX_TARGETS {
            break;
        }
        if seen.insert(ip) {
            targets.push(ip);
        }
    }
    targets
}

/// Adds peers of an overlay network that installs host routes (Tailscale today)
/// so the scan can find a resolver in another `/32`.
///
/// This is best-effort on purpose: a missing or not-running CLI means the scan
/// simply stays on the local network, and other overlays (WireGuard, ZeroTier)
/// are still reached by `ytui discover --url`.
fn add_overlay_peers(push: &mut impl FnMut(IpAddr)) {
    let Some(status) = tailscale_status() else {
        return;
    };
    if let Some(node) = status.get("Self") {
        add_tailscale_node(node, push);
    }
    if let Some(peers) = status.get("Peer").and_then(|v| v.as_object()) {
        for node in peers.values() {
            add_tailscale_node(node, push);
        }
    }
}

fn add_tailscale_node(node: &serde_json::Value, push: &mut impl FnMut(IpAddr)) {
    // A node that has been offline for weeks still has a route, but answering
    // is hopeless and waiting for it would eat the scan budget.
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

/// `tailscale status --json`, also trying the Windows executable name so a
/// client inside WSL can see the host's tailnet, and the macOS app bundle when
/// the CLI is not on `PATH`. Any failure is a quiet "no overlay peers".
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
        let Some(out) = bounded_output(cmd, &["status", "--json"], OVERLAY_CLI_TIMEOUT) else {
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

/// Runs a command and collects stdout, killing it after `timeout`.
///
/// `Command::output` would wait forever on a wedged daemon, and a plain
/// `try_wait` poll can deadlock on a full pipe, so stdout is drained on its own
/// thread while the parent watches the clock.
fn bounded_output(program: &str, args: &[&str], timeout: Duration) -> Option<std::process::Output> {
    use std::io::Read;

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        buf
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let stdout = reader.join().ok()?;
    Some(std::process::Output {
        status,
        stdout,
        stderr: Vec::new(),
    })
}

/// Overlay peers live in `100.64.0.0/10`, outside the machine's own private
/// `/24`, and are reached through a tunnel. They get the longer probe timeout,
/// as do IPv6 targets, which are either a local ULA or a peer's tailnet address.
fn probe_timeout(ip: IpAddr) -> Duration {
    let overlay = match ip {
        IpAddr::V4(v4) => {
            let range = u32::from(v4) & 0xffc0_0000;
            range == u32::from(Ipv4Addr::new(100, 64, 0, 0))
        }
        IpAddr::V6(_) => true,
    };
    if overlay {
        OVERLAY_PROBE_TIMEOUT
    } else {
        PROBE_TIMEOUT
    }
}

/// VMware, Hyper-V, WSL and VirtualBox give the host the first address of
/// their own /24. A resolver almost never lives on one, so probe them last
/// instead of dropping them.
fn host_only_switch(ip: Ipv4Addr) -> bool {
    ip.octets()[3] == 1
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
        solve_ms: health.resolve_ms,
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
    fn overlay_peers_get_the_longer_probe_timeout() {
        let peer = IpAddr::V4(Ipv4Addr::new(100, 126, 49, 124));
        assert_eq!(probe_timeout(peer), OVERLAY_PROBE_TIMEOUT);
        assert_eq!(
            probe_timeout(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5))),
            PROBE_TIMEOUT
        );
        assert_eq!(
            probe_timeout(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)),
            OVERLAY_PROBE_TIMEOUT
        );
    }

    #[test]
    fn tailscale_nodes_add_online_v4_and_v6_peers_only() {
        let online: serde_json::Value = serde_json::from_str(
            r#"{"Online":true,"TailscaleIPs":["100.126.49.124","fd7a:115c:a1e0::1"]}"#,
        )
        .unwrap();
        let offline: serde_json::Value =
            serde_json::from_str(r#"{"Online":false,"TailscaleIPs":["100.76.39.13"]}"#).unwrap();
        let mut ips = Vec::new();
        add_tailscale_node(&online, &mut |ip| ips.push(ip));
        add_tailscale_node(&offline, &mut |ip| ips.push(ip));
        assert_eq!(
            ips,
            vec![
                IpAddr::V4(Ipv4Addr::new(100, 126, 49, 124)),
                "fd7a:115c:a1e0::1".parse().unwrap(),
            ]
        );
    }

    #[test]
    fn a_tailscale_node_without_an_online_flag_is_kept() {
        // The Self entry has no Online field; it must not be dropped.
        let node: serde_json::Value =
            serde_json::from_str(r#"{"TailscaleIPs":["100.68.31.91"]}"#).unwrap();
        let mut ips = Vec::new();
        add_tailscale_node(&node, &mut |ip| ips.push(ip));
        assert_eq!(ips, vec![IpAddr::V4(Ipv4Addr::new(100, 68, 31, 91))]);
    }

    #[test]
    fn classifies_the_configured_resolver() {
        for url in [
            "http://127.0.0.1:8787",
            "http://[::1]:8787",
            "http://localhost:8787",
        ] {
            assert_eq!(reach(url), Reach::Local, "{url}");
        }
        for url in [
            "http://192.168.1.170:8787",
            "http://10.0.0.5:8787",
            "http://100.64.0.5:8787",
            "http://raspberrypi.local:8787",
        ] {
            assert_eq!(reach(url), Reach::Lan, "{url}");
        }
        for url in ["https://resolver.example.com:8787", "http://8.8.8.8:8787"] {
            assert_eq!(reach(url), Reach::Public, "{url}");
        }
    }

    #[test]
    fn a_faster_resolver_outranks_a_nearer_slow_one() {
        let slow = Found {
            url: "http://10.0.0.2:8787".into(),
            version: "0.4.4".into(),
            auth: false,
            ms: 5,
            solve_ms: Some(8000),
        };
        let fast = Found {
            url: "http://10.0.0.3:8787".into(),
            version: "0.4.4".into(),
            auth: false,
            ms: 40,
            solve_ms: Some(2000),
        };
        let mut found = vec![slow, fast];
        found.sort_by(rank);
        assert_eq!(found[0].url, "http://10.0.0.3:8787");
    }

    #[test]
    fn host_only_switches_are_recognized() {
        assert!(host_only_switch(Ipv4Addr::new(172, 27, 240, 1)));
        assert!(host_only_switch(Ipv4Addr::new(192, 168, 56, 1)));
        assert!(!host_only_switch(Ipv4Addr::new(192, 168, 1, 137)));
        // A real LAN host can be .1 too; the flag only changes scan order.
        assert!(host_only_switch(Ipv4Addr::new(192, 168, 1, 1)));
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
