//! `ytui-resolver`: a small HTTP server that runs yt-dlp on another machine.
//!
//! The client still finds tracks, lists playlists and plays audio; this server
//! only turns a watch URL into AAC candidates. That moves yt-dlp's ~110 MB and
//! deno's ~300 MB peak off the laptop.
//!
//! Protocol v1:
//!   GET /v1/health                                   (no token)
//!   GET /v1/resolve?url=<watch-url>[&refresh=1]      (bearer token)
//!
//! There is deliberately no TLS here. Run it on a LAN, Tailscale or WireGuard,
//! or put a reverse proxy in front when it has to be exposed.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

use ytui_resolve::{Candidate, Resolved};

mod service;
mod update;

const DEFAULT_BIND: &str = "127.0.0.1:8787";
const DEFAULT_TTL: Duration = Duration::from_secs(4 * 60 * 60);
const DEFAULT_CACHE_CAP: usize = 512;
const DEFAULT_CONCURRENCY: usize = 2;
const RATE_MAX: usize = 60;
const RATE_WINDOW: Duration = Duration::from_secs(60);

fn main() {
    if let Err(e) = run() {
        eprintln!("ytui-resolver: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().collect();
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        help();
        return Ok(());
    }
    if argv.iter().any(|a| a == "-V" || a == "--version") {
        println!("ytui-resolver {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if argv.get(1).map(String::as_str) == Some("update") {
        return update::cli(&argv[2..]);
    }

    let args = Args::parse(&argv)?;

    if args.uninstall_service {
        return service::uninstall();
    }
    if args.install_service {
        return install_service(&args);
    }
    if args.gen_token {
        let path = args.token_file.clone().unwrap_or_else(default_token_path);
        write_secret(&path, &generate_token())?;
        println!("wrote token to {}", path.display());
        println!("point the client at it with resolver_token_file in config.json");
        return Ok(());
    }

    let token = load_token(&args)?;
    check_bind_auth(&args.bind, token.is_some(), args.allow_private)?;
    if let Some(p) = &args.cookies {
        if !p.exists() {
            eprintln!("warning: cookie file {} does not exist", p.display());
        }
    }
    if token.is_none() {
        if args.allow_private {
            eprintln!("no token configured; only private LAN/Tailscale peers are accepted");
        } else {
            eprintln!("no token configured (loopback only)");
        }
    }
    if args.allow_private {
        eprintln!("private LAN/Tailscale peers do not need a token");
    }

    let ctx = Arc::new(Ctx {
        token,
        allow_private: args.allow_private,
        cookies: args.cookies,
        cache: Mutex::new(Cache::new(args.cache_cap, args.cache_ttl)),
        rate: Mutex::new(RateLimit::new(RATE_MAX, RATE_WINDOW)),
        tools: Mutex::new(None),
        speed: Mutex::new(Speed::default()),
    });
    let gate = Arc::new(Gate::new(args.max_concurrency));
    let server =
        Arc::new(Server::http(&args.bind).map_err(|e| format!("cannot bind {}: {e}", args.bind))?);

    eprintln!(
        "ytui-resolver {} listening on http://{} (cache {} entries / {}s, {} concurrent resolves)",
        env!("CARGO_PKG_VERSION"),
        args.bind,
        args.cache_cap,
        args.cache_ttl.as_secs(),
        args.max_concurrency
    );

    if args.auto_update {
        update::spawn_auto();
    }

    loop {
        match server.recv() {
            Ok(request) => {
                // Health is cheap and never runs yt-dlp, so it must not wait
                // behind a full resolve queue (doctor would look unreachable).
                let health = request.url().starts_with("/v1/health");
                if health {
                    let ctx = Arc::clone(&ctx);
                    thread::spawn(move || handle(request, &ctx));
                    continue;
                }
                // Take the slot before spawning, so a burst queues in the
                // socket instead of starting an unbounded number of threads.
                let permit = gate.acquire();
                let ctx = Arc::clone(&ctx);
                thread::spawn(move || {
                    handle(request, &ctx);
                    drop(permit);
                });
            }
            Err(e) => eprintln!("accept: {e}"),
        }
    }
}

struct Args {
    bind: String,
    token: Option<String>,
    token_file: Option<PathBuf>,
    cookies: Option<PathBuf>,
    cache_ttl: Duration,
    cache_cap: usize,
    max_concurrency: usize,
    allow_private: bool,
    gen_token: bool,
    install_service: bool,
    uninstall_service: bool,
    /// Check github every four hours and reinstall when a newer version is
    /// there. On by default; `--no-auto-update` turns it off.
    auto_update: bool,
}

impl Args {
    fn parse(argv: &[String]) -> Result<Self, String> {
        let mut a = Self {
            bind: DEFAULT_BIND.into(),
            token: None,
            token_file: None,
            cookies: None,
            cache_ttl: DEFAULT_TTL,
            cache_cap: DEFAULT_CACHE_CAP,
            max_concurrency: DEFAULT_CONCURRENCY,
            allow_private: false,
            gen_token: false,
            install_service: false,
            uninstall_service: false,
            auto_update: true,
        };
        let mut i = 1;
        while i < argv.len() {
            let arg = argv[i].as_str();
            let mut value = |name: &str| -> Result<String, String> {
                i += 1;
                argv.get(i)
                    .cloned()
                    .ok_or_else(|| format!("{name} needs a value"))
            };
            match arg {
                "--bind" => a.bind = value("--bind")?,
                "--token" => a.token = Some(value("--token")?),
                "--token-file" => a.token_file = Some(PathBuf::from(value("--token-file")?)),
                "--cookies" => a.cookies = Some(PathBuf::from(value("--cookies")?)),
                "--cache-ttl-secs" => {
                    let n: u64 = value("--cache-ttl-secs")?
                        .parse()
                        .map_err(|_| "--cache-ttl-secs must be a number")?;
                    a.cache_ttl = Duration::from_secs(n);
                }
                "--cache-cap" => {
                    a.cache_cap = value("--cache-cap")?
                        .parse()
                        .map_err(|_| "--cache-cap must be a number")?;
                }
                "--max-concurrency" => {
                    a.max_concurrency = value("--max-concurrency")?
                        .parse::<usize>()
                        .map_err(|_| "--max-concurrency must be a number")?
                        .max(1);
                }
                "--gen-token" => a.gen_token = true,
                "--allow-private" => a.allow_private = true,
                "--install-service" => a.install_service = true,
                "--uninstall-service" => a.uninstall_service = true,
                "--auto-update" => a.auto_update = true,
                "--no-auto-update" => a.auto_update = false,
                other => return Err(format!("unknown option {other} (try --help)")),
            }
            i += 1;
        }
        Ok(a)
    }
}

/// Non-loopback binds need either a token or explicit trust in private peers.
fn check_bind_auth(bind: &str, has_token: bool, allow_private: bool) -> Result<(), String> {
    if bind_is_loopback(bind) || has_token || allow_private {
        return Ok(());
    }
    Err(
        "refusing to listen on a non-loopback address without a token\n\
         pass --allow-private to trust LAN/Tailscale peers without one, or\n\
         generate a token with --gen-token"
            .into(),
    )
}

/// Writes the autostart entry for this machine, reusing the options given on
/// this command line, and starts it where the platform does that.
fn install_service(args: &Args) -> Result<(), String> {
    if args.gen_token {
        return Err("--install-service and --gen-token are separate steps".into());
    }
    let token = load_token(args)?;
    check_bind_auth(&args.bind, token.is_some(), args.allow_private)?;

    // Keep the secret out of the unit file: an inline token moves to the
    // default 0600 file, and the service only ever sees the path.
    let token_path = if let Some(t) = &args.token {
        let path = default_token_path();
        write_secret(&path, t)?;
        println!("wrote token to {}", path.display());
        Some(path)
    } else if let Some(p) = &args.token_file {
        Some(p.clone())
    } else if default_token_path().exists() {
        Some(default_token_path())
    } else {
        None
    };

    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot find this binary to autostart it: {e}"))?;
    let spec_args = service_args(args, token_path);

    if bind_is_loopback(&args.bind) {
        println!(
            "note: {} is loopback only; pass --bind 0.0.0.0:8787 for remote clients",
            args.bind
        );
    }
    service::install(&service::Spec {
        exe,
        args: spec_args,
    })
}

/// The flags the service should run with: what the user asked for, without the
/// service flags and with defaults omitted so the unit file stays readable.
fn service_args(args: &Args, token_path: Option<PathBuf>) -> Vec<String> {
    let mut out = vec!["--bind".to_string(), args.bind.clone()];
    if let Some(p) = token_path {
        out.push("--token-file".into());
        out.push(p.display().to_string());
    }
    if args.allow_private {
        out.push("--allow-private".into());
    }
    if let Some(p) = &args.cookies {
        out.push("--cookies".into());
        out.push(p.display().to_string());
    }
    if args.cache_ttl != DEFAULT_TTL {
        out.push("--cache-ttl-secs".into());
        out.push(args.cache_ttl.as_secs().to_string());
    }
    if args.cache_cap != DEFAULT_CACHE_CAP {
        out.push("--cache-cap".into());
        out.push(args.cache_cap.to_string());
    }
    if args.max_concurrency != DEFAULT_CONCURRENCY {
        out.push("--max-concurrency".into());
        out.push(args.max_concurrency.to_string());
    }
    if !args.auto_update {
        out.push("--no-auto-update".into());
    }
    out
}

fn help() {
    println!(
        "\
ytui-resolver {}

Runs yt-dlp for ytui clients on another machine.

usage:
  ytui-resolver [options]
  ytui-resolver update [--check] [--force]   reinstall from github

options:
  --bind ADDR             listen address (default {DEFAULT_BIND})
  --token TOKEN           bearer token clients must send
  --token-file PATH       read the token from PATH
  --gen-token             create a random token and exit
  --allow-private         trust LAN/Tailscale peers without a token
  --install-service       start at login (systemd, launchd or Startup)
  --uninstall-service     remove that autostart entry and stop it
  --cookies PATH          pass cookies.txt to yt-dlp (age-restricted tracks)
  --cache-ttl-secs N      how long resolved URLs are reused (default 14400)
  --cache-cap N           cached videos kept (default 512)
  --max-concurrency N     simultaneous yt-dlp runs (default 2)
  --auto-update           check github every 4 hours and reinstall (default)
  --no-auto-update        do not update the server on its own
  -V, --version           print version
  -h, --help              this text

  ytui-resolver update        install a newer version from github
  ytui-resolver update --check  only report whether one exists

The default bind is loopback. Bind a tailnet/LAN address for remote clients,
and either use --allow-private (trust private ranges) or a token. Public
peers always need the token. TLS is left to Tailscale/WireGuard or a proxy.

With auto-update on, the running server checks github every four hours and
reinstalls itself in place with `cargo install --git`, then restarts into the
new binary. Linux and macOS only; `--no-auto-update` turns it off.

--install-service reuses the other flags, so install it the way you run it:

  ytui-resolver --bind 0.0.0.0:8787 --allow-private --install-service",
        env!("CARGO_PKG_VERSION")
    );
}

fn load_token(args: &Args) -> Result<Option<String>, String> {
    if let Some(t) = &args.token {
        return Ok(Some(t.clone()));
    }
    let path = match &args.token_file {
        Some(p) => Some(p.clone()),
        None => {
            let p = default_token_path();
            p.exists().then_some(p)
        }
    };
    match path {
        Some(p) => {
            let t = fs::read_to_string(&p)
                .map_err(|e| format!("cannot read token file {}: {e}", p.display()))?
                .trim()
                .to_string();
            if t.is_empty() {
                return Err(format!("token file {} is empty", p.display()));
            }
            Ok(Some(t))
        }
        None => Ok(None),
    }
}

fn default_token_path() -> PathBuf {
    home().join(".config").join("ytui").join("resolver.token")
}

fn home() -> PathBuf {
    #[cfg(windows)]
    let vars: &[&str] = &["USERPROFILE", "HOME"];
    #[cfg(not(windows))]
    let vars: &[&str] = &["HOME"];

    for var in vars {
        if let Some(home) = std::env::var_os(var) {
            if !home.is_empty() {
                return PathBuf::from(home);
            }
        }
    }
    PathBuf::from(".")
}

fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).expect("no system randomness");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn write_secret(path: &Path, value: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let mut file =
        fs::File::create(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    file.write_all(value.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn bind_is_loopback(bind: &str) -> bool {
    let host = bind.rsplit_once(':').map(|(h, _)| h).unwrap_or(bind);
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

struct Ctx {
    token: Option<String>,
    allow_private: bool,
    cookies: Option<PathBuf>,
    cache: Mutex<Cache>,
    rate: Mutex<RateLimit>,
    tools: Mutex<Option<(Instant, Option<String>, Option<String>)>>,
    speed: Mutex<Speed>,
}

fn handle(request: Request, ctx: &Ctx) {
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("");
    match path {
        "/v1/health" => {
            if request.method() != &Method::Get {
                send_text(request, 405, "method not allowed");
                return;
            }
            let body = health_json(ctx, &request);
            send_json(request, 200, body);
        }
        "/v1/resolve" => resolve_request(request, &url, ctx),
        _ => send_text(request, 404, "not found"),
    }
}

fn resolve_request(request: Request, url: &str, ctx: &Ctx) {
    if request.method() != &Method::Get {
        send_text(request, 405, "method not allowed");
        return;
    }
    if !authorized(&request, &ctx.token, ctx.allow_private) {
        send_json(request, 401, r#"{"error":"bad token"}"#.into());
        return;
    }
    let peer = request
        .remote_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "local".into());
    if !ctx
        .rate
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow(&peer)
    {
        send_json(request, 429, r#"{"error":"rate limited"}"#.into());
        return;
    }

    let query = parse_query(url);
    let Some(raw) = query.get("url") else {
        send_json(request, 400, r#"{"error":"missing url parameter"}"#.into());
        return;
    };
    let Some(id) = video_id(raw) else {
        send_json(
            request,
            400,
            r#"{"error":"url must be a YouTube watch or youtu.be link"}"#.into(),
        );
        return;
    };
    let refresh = matches!(query.get("refresh").map(String::as_str), Some("1" | "true"));

    if !refresh {
        let hit = ctx.cache.lock().unwrap_or_else(|e| e.into_inner()).get(&id);
        if let Some(track) = hit {
            send_json(request, 200, resolve_json(&track, 0));
            return;
        }
    }

    let canonical = format!("https://www.youtube.com/watch?v={id}");
    let cookies = ctx
        .cookies
        .as_deref()
        .and_then(Path::to_str)
        .map(|p| ("--cookies", p));
    let started = Instant::now();
    let result = ytui_resolve::resolve(&canonical, cookies);
    let ms = started.elapsed().as_millis() as u64;

    match result {
        Ok(track) => {
            ctx.cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .put(&id, track.clone());
            ctx.speed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .record(ms);
            eprintln!(
                "resolve {id} ok ({ms} ms, {} candidates)",
                track.candidates.len()
            );
            send_json(request, 200, resolve_json(&track, ms));
        }
        Err(e) => {
            eprintln!("resolve {id} failed: {e}");
            let body = serde_json::json!({ "error": e }).to_string();
            send_json(request, 502, body);
        }
    }
}

fn authorized(request: &Request, token: &Option<String>, allow_private: bool) -> bool {
    let private = request
        .remote_addr()
        .map(|a| ytui_resolve::is_private_ip(a.ip()))
        .unwrap_or(true);
    // Explicitly trusted private ranges skip the token. A loopback-only bind
    // never sees a public peer, so no token is needed there either.
    if private && allow_private {
        return true;
    }
    let Some(expected) = token else {
        return private;
    };
    let got = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .map(|h| h.value.as_str())
        .unwrap_or("");
    let got = got
        .strip_prefix("Bearer ")
        .or_else(|| got.strip_prefix("bearer "))
        .unwrap_or("")
        .trim();
    constant_time_eq(got.as_bytes(), expected.as_bytes())
}

/// Compare without returning early on the first differing byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

fn health_json(ctx: &Ctx, request: &Request) -> String {
    let private = request
        .remote_addr()
        .map(|a| ytui_resolve::is_private_ip(a.ip()))
        .unwrap_or(true);
    let auth = token_needed(ctx.token.is_some(), ctx.allow_private, private);
    let resolve_ms = ctx
        .speed
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .median();
    let (ytdlp, deno) = tool_versions(ctx);
    serde_json::json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "ytdlp": ytdlp,
        "deno": deno,
        "auth": auth,
        // Whether the resolver passes a cookie file of its own to yt-dlp. The
        // player's cookies are never sent over the network.
        "cookies": ctx.cookies.is_some(),
        // Recent cold-resolve time, so clients with more than one resolver on
        // the network can pick the fastest rather than the nearest.
        "resolve_ms": resolve_ms,
    })
    .to_string()
}

/// Tool versions for `/v1/health`, cached for five minutes. Spawning `yt-dlp`
/// and `deno` takes about a second on a Raspberry Pi, which used to make the
/// health probe slow enough for a client scan to give up on it.
fn tool_versions(ctx: &Ctx) -> (Option<String>, Option<String>) {
    const TTL: Duration = Duration::from_secs(5 * 60);
    let mut cache = ctx.tools.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((at, ytdlp, deno)) = cache.as_ref() {
        if at.elapsed() < TTL {
            return (ytdlp.clone(), deno.clone());
        }
    }
    let fresh = (version_of("yt-dlp"), version_of("deno"));
    *cache = Some((Instant::now(), fresh.0.clone(), fresh.1.clone()));
    fresh
}

/// Whether `authorized` would reject a request from this peer without a token.
/// Kept separate from the request so the rule can be tested directly.
fn token_needed(has_token: bool, allow_private: bool, private: bool) -> bool {
    if allow_private && private {
        false
    } else if has_token {
        true
    } else {
        !private
    }
}

fn version_of(bin: &str) -> Option<String> {
    let out = Command::new(bin)
        .env("PATH", ytui_resolve::tool_path())
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn resolve_json(track: &Resolved, ms: u64) -> String {
    #[derive(Serialize)]
    struct Body<'a> {
        v: u8,
        id: &'a str,
        title: &'a str,
        channel: &'a str,
        duration: f64,
        ms: u64,
        candidates: &'a [Candidate],
    }
    serde_json::to_string(&Body {
        v: 1,
        id: &track.id,
        title: &track.title,
        channel: &track.channel,
        duration: track.duration,
        ms,
        candidates: &track.candidates,
    })
    .unwrap_or_else(|_| r#"{"error":"cannot serialize response"}"#.into())
}

fn send_json(request: Request, code: u16, body: String) {
    send(request, code, body, "application/json");
}

fn send_text(request: Request, code: u16, body: &str) {
    send(request, code, body.to_string(), "text/plain; charset=utf-8");
}

fn send(request: Request, code: u16, body: String, content_type: &str) {
    let header = Header::from_bytes(&b"Content-Type"[..], content_type.as_bytes())
        .expect("static content type");
    let response = Response::from_string(body)
        .with_status_code(StatusCode(code))
        .with_header(header);
    let _ = request.respond(response);
}

// ---------------- cache ----------------

struct Cache {
    entries: HashMap<String, (Instant, Resolved)>,
    cap: usize,
    ttl: Duration,
}

impl Cache {
    fn new(cap: usize, ttl: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            cap: cap.max(1),
            ttl,
        }
    }

    fn get(&mut self, id: &str) -> Option<Resolved> {
        let expired = self
            .entries
            .get(id)
            .map(|(at, _)| at.elapsed() > self.ttl)
            .unwrap_or(false);
        if expired {
            self.entries.remove(id);
            return None;
        }
        self.entries.get(id).map(|(_, track)| track.clone())
    }

    fn put(&mut self, id: &str, track: Resolved) {
        if self.entries.len() >= self.cap {
            if let Some(old) = self
                .entries
                .iter()
                .min_by_key(|(_, (at, _))| *at)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&old);
            }
        }
        self.entries.insert(id.to_string(), (Instant::now(), track));
    }
}

// ---------------- resolve speed ----------------

/// Recent cold-resolve times, reported by `/v1/health` so a client with more
/// than one resolver on the network can pick the fastest, not the nearest.
#[derive(Default)]
struct Speed {
    samples: VecDeque<u64>,
}

/// Resolves kept for the median; enough to smooth a slow first run.
const SPEED_SAMPLES: usize = 8;

impl Speed {
    fn record(&mut self, ms: u64) {
        if self.samples.len() == SPEED_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(ms);
    }

    fn median(&self) -> Option<u64> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<u64> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        Some(sorted[sorted.len() / 2])
    }
}

// ---------------- rate limit ----------------

struct RateLimit {
    hits: HashMap<String, VecDeque<Instant>>,
    max: usize,
    window: Duration,
}

impl RateLimit {
    fn new(max: usize, window: Duration) -> Self {
        Self {
            hits: HashMap::new(),
            max,
            window,
        }
    }

    fn allow(&mut self, key: &str) -> bool {
        let now = Instant::now();
        let hits = self.hits.entry(key.to_string()).or_default();
        while hits
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) > self.window)
        {
            hits.pop_front();
        }
        if hits.len() >= self.max {
            false
        } else {
            hits.push_back(now);
            true
        }
    }
}

// ---------------- concurrency gate ----------------

struct Gate {
    limit: usize,
    active: Mutex<usize>,
    cv: Condvar,
}

impl Gate {
    fn new(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            active: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> Permit {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        while *active >= self.limit {
            active = self.cv.wait(active).unwrap_or_else(|e| e.into_inner());
        }
        *active += 1;
        Permit(Arc::clone(self))
    }
}

struct Permit(Arc<Gate>);

impl Drop for Permit {
    fn drop(&mut self) {
        let mut active = self.0.active.lock().unwrap_or_else(|e| e.into_inner());
        *active = active.saturating_sub(1);
        self.0.cv.notify_one();
    }
}

// ---------------- URL parsing ----------------

fn parse_query(url: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some((_, query)) = url.split_once('?') {
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            out.insert(pct_decode(k), pct_decode(v));
        }
    }
    out
}

fn pct_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(a), Some(b)) => {
                    out.push(a * 16 + b);
                    i += 3;
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Accepts a YouTube watch/shorts/live/embed URL or a youtu.be link and
/// returns the 11-character video id. Everything else is refused, so the
/// server never passes arbitrary text to yt-dlp.
fn video_id(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let no_scheme = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or(raw);
    let (authority, rest) = no_scheme.split_once('/').unwrap_or((no_scheme, ""));
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let short = host == "youtu.be";
    let youtube = matches!(host, "youtube.com" | "music.youtube.com" | "m.youtube.com");
    if !short && !youtube {
        return None;
    }

    let path = rest.split(['?', '#']).next().unwrap_or("");
    let candidate = if short {
        path.split('/').next().unwrap_or("").to_string()
    } else if path == "watch" || path.is_empty() {
        let (_, query) = rest.split_once('?')?;
        parse_query(&format!("/?{query}")).get("v")?.clone()
    } else {
        let mut parts = path.split('/');
        match parts.next()? {
            "shorts" | "live" | "embed" => parts.next()?.to_string(),
            _ => return None,
        }
    };

    is_video_id(&candidate).then_some(candidate)
}

fn is_video_id(s: &str) -> bool {
    s.len() == 11
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_watch_and_short_links() {
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=LM",
            "http://m.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?t=42",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ",
        ] {
            assert_eq!(video_id(url).as_deref(), Some("dQw4w9WgXcQ"), "{url}");
        }
    }

    #[test]
    fn rejects_everything_else() {
        for url in [
            "https://example.com/watch?v=dQw4w9WgXcQ",
            "https://www.youtube.com/playlist?list=PL123",
            "https://www.youtube.com/watch?v=short",
            "https://www.youtube.com/redirect?q=--exec",
            "file:///etc/passwd",
            "",
        ] {
            assert_eq!(video_id(url), None, "{url}");
        }
    }

    #[test]
    fn drops_extra_query_parameters() {
        assert_eq!(
            video_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&x=--exec").as_deref(),
            Some("dQw4w9WgXcQ")
        );
    }

    #[test]
    fn decodes_query_values() {
        let q = parse_query(
            "/v1/resolve?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3Dabc&refresh=1",
        );
        assert_eq!(
            q.get("url").map(String::as_str),
            Some("https://www.youtube.com/watch?v=abc")
        );
        assert_eq!(q.get("refresh").map(String::as_str), Some("1"));
    }

    #[test]
    fn constant_time_compare_is_correct() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secret "));
    }

    #[test]
    fn cache_expires_and_evicts() {
        let mut cache = Cache::new(1, Duration::from_millis(20));
        let track = Resolved {
            id: "a".into(),
            title: "t".into(),
            channel: "c".into(),
            duration: 1.0,
            candidates: Vec::new(),
        };
        cache.put("a", track.clone());
        assert!(cache.get("a").is_some());
        cache.put("b", track);
        assert!(cache.get("a").is_none());
        assert!(cache.get("b").is_some());
        thread::sleep(Duration::from_millis(30));
        assert!(cache.get("b").is_none());
    }

    #[test]
    fn speed_reports_a_median() {
        let mut speed = Speed::default();
        assert_eq!(speed.median(), None);
        speed.record(9000);
        assert_eq!(speed.median(), Some(9000));
        speed.record(1000);
        speed.record(5000);
        assert_eq!(speed.median(), Some(5000));
        for ms in [2000, 3000, 4000, 6000, 7000, 8000] {
            speed.record(ms);
        }
        assert_eq!(speed.samples.len(), SPEED_SAMPLES);
        assert_eq!(speed.median(), Some(5000));
    }

    #[test]
    fn rate_limit_window() {
        let mut rate = RateLimit::new(2, Duration::from_millis(20));
        assert!(rate.allow("peer"));
        assert!(rate.allow("peer"));
        assert!(!rate.allow("peer"));
        thread::sleep(Duration::from_millis(30));
        assert!(rate.allow("peer"));
    }

    #[test]
    fn token_requirements_follow_the_private_trust_rule() {
        // No token, private trust: private peers are fine, public ones are not.
        assert!(!token_needed(false, true, true));
        assert!(token_needed(false, true, false));
        // Loopback-only server: the peer is private and no token exists.
        assert!(!token_needed(false, false, true));
        // A token protects everyone unless the peer is explicitly trusted.
        assert!(token_needed(true, false, true));
        assert!(token_needed(true, false, false));
        assert!(!token_needed(true, true, true));
        assert!(token_needed(true, true, false));
    }

    #[test]
    fn loopback_bind_detection() {
        assert!(bind_is_loopback("127.0.0.1:8787"));
        assert!(bind_is_loopback("localhost:8787"));
        assert!(bind_is_loopback("[::1]:8787"));
        assert!(!bind_is_loopback("0.0.0.0:8787"));
        assert!(!bind_is_loopback("100.64.0.5:8787"));
    }

    fn base_args() -> Args {
        Args::parse(&["ytui-resolver".to_string()]).unwrap()
    }

    #[test]
    fn service_args_keep_the_options_that_matter() {
        let mut args = base_args();
        args.bind = "0.0.0.0:8787".into();
        args.allow_private = true;
        assert_eq!(
            service_args(&args, None),
            vec!["--bind", "0.0.0.0:8787", "--allow-private"]
        );
    }

    #[test]
    fn service_args_use_a_token_file_not_the_secret() {
        let mut args = base_args();
        args.token = Some("hunter2".into());
        let path = PathBuf::from("/home/me/.config/ytui/resolver.token");
        let spec = service_args(&args, Some(path.clone()));
        assert!(spec
            .windows(2)
            .any(|w| w == ["--token-file", path.to_str().unwrap()]));
        assert!(!spec.iter().any(|a| a.contains("hunter2")));
    }

    #[test]
    fn service_args_include_changed_tunables_only() {
        let mut args = base_args();
        args.cache_cap = 64;
        args.max_concurrency = 4;
        let spec = service_args(&args, None);
        assert_eq!(
            spec,
            vec!["--bind", DEFAULT_BIND, "--cache-cap", "64", "--max-concurrency", "4"]
        );
    }

    #[test]
    fn auto_update_is_on_unless_told_otherwise() {
        let on = Args::parse(&["ytui-resolver".to_string()]).unwrap();
        assert!(on.auto_update);
        assert!(!service_args(&on, None).contains(&"--no-auto-update".to_string()));

        let off = Args::parse(&[
            "ytui-resolver".to_string(),
            "--no-auto-update".to_string(),
        ])
        .unwrap();
        assert!(!off.auto_update);
        assert_eq!(
            service_args(&off, None),
            vec!["--bind", DEFAULT_BIND, "--no-auto-update"]
        );
    }
}
