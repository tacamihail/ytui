//! The optional web player: a small static page the resolver can serve.
//!
//! Everything web sits behind `--web`, which is off by default, so an
//! existing deployment is unchanged. The routes follow the rules of the rest
//! of the server: token or `--allow-private`, rate-limited per peer.
//!
//!   GET  /                the player page (one file, no build step)
//!   GET  /v1/session      auth state and whether the server has cookies
//!   POST /v1/login        token -> HttpOnly session cookie
//!   GET  /v1/search?q=    search through the shared listing code
//!   GET  /v1/liked        Liked Music through the shared listing code
//!   GET  /v1/playlists    the signed-in account's playlists
//!   GET  /v1/playlist?id= tracks in one playlist
//!   GET  /audio/<id>      Range proxy to googlevideo
//!
//! The audio is proxied rather than linked: googlevideo URLs can be bound to
//! the requesting IP, so a URL resolved on this server can 403 when a phone
//! fetches it directly. The proxy also keeps the stream URLs private and
//! reuses the resolve cache. A stream answers for the whole track, so the
//! main loop never holds a resolve slot for it; [`resolve_id`] takes the
//! gate itself, and only for the cold resolve.

use std::io::Read;
use std::time::{Duration, Instant};

use serde_json::json;
use tiny_http::{Header, Method, Request, Response, StatusCode};
use ytui_resolve::listing::{self, Kind, TrackMeta, LIKED_MUSIC_URL, WATCH_LATER_URL};
use ytui_resolve::{Resolved, UA};

use crate::{constant_time_eq, parse_query, send, send_json, send_text, Ctx};

const INDEX_HTML: &str = include_str!("../web/index.html");

/// How long a listing answer is reused. The player caches tabs for the same
/// span; a phone re-fetches on every visit, so this also keeps yt-dlp off
/// the hot path.
const LISTING_TTL: Duration = Duration::from_secs(15 * 60);

/// Rows pulled per listing; the player's `fetch_limit` default.
const WEB_FETCH_LIMIT: u32 = 2000;

/// Range requests are how `<audio>` seeks and buffers, so the proxy gets a
/// wider bucket than the one resolve per track the JSON routes need.
pub(crate) const AUDIO_RATE_MAX: usize = 240;

/// How many candidates the proxy tries before it re-resolves once.
const MAX_TRIES: usize = 8;

/// Connect and per-read limits for streaming. There is deliberately no overall
/// timeout: a long mix keeps one connection open for minutes, but a stalled
/// socket must not hang a slot forever.
const STREAM_CONNECT: Duration = Duration::from_secs(10);
const STREAM_READ: Duration = Duration::from_secs(30);

fn stream_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(STREAM_CONNECT)
        .timeout_read(STREAM_READ)
        .build()
}

pub fn page(request: Request) {
    send(request, 200, INDEX_HTML.to_string(), "text/html; charset=utf-8");
}

pub fn session_request(request: Request, ctx: &Ctx) {
    if !crate::authorized(&request, &ctx.token, ctx.allow_private) {
        send_json(request, 401, r#"{"error":"bad token"}"#.into());
        return;
    }
    send_json(
        request,
        200,
        json!({ "auth": true, "cookies": ctx.cookies.is_some() }).to_string(),
    );
}

pub fn login_request(mut request: Request, ctx: &Ctx) {
    if request.method() != &Method::Post {
        send_text(request, 405, "method not allowed");
        return;
    }
    // A tokenless server trusts private peers; there is nothing to exchange.
    let Some(expected) = &ctx.token else {
        send_json(request, 200, r#"{"auth":true,"tokenless":true}"#.into());
        return;
    };
    // 4 KiB is far beyond the 64 hex characters a token is.
    let mut body = String::new();
    if request
        .as_reader()
        .take(4096)
        .read_to_string(&mut body)
        .is_err()
    {
        send_json(request, 400, r#"{"error":"bad login body"}"#.into());
        return;
    }
    let got = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("token").and_then(|t| t.as_str()).map(String::from))
        .unwrap_or_default();
    if !constant_time_eq(got.as_bytes(), expected.as_bytes()) {
        send_json(request, 401, r#"{"error":"bad token"}"#.into());
        return;
    }
    // HttpOnly so a stray script cannot read the session. TLS is left to the
    // front (tailscale serve or a proxy), same as the rest of the server.
    let cookie = format!("ytui_web={got}; Path=/; Max-Age=31536000; HttpOnly; SameSite=Strict");
    let header = Header::from_bytes(&b"Set-Cookie"[..], cookie.as_bytes()).expect("static header");
    let response = Response::from_string(r#"{"auth":true}"#)
        .with_status_code(StatusCode(200))
        .with_header(header);
    let _ = request.respond(response);
}

pub fn search_request(request: Request, url: &str, ctx: &Ctx) {
    if request.method() != &Method::Get {
        send_text(request, 405, "method not allowed");
        return;
    }
    if !crate::authorized(&request, &ctx.token, ctx.allow_private) {
        send_json(request, 401, r#"{"error":"bad token"}"#.into());
        return;
    }
    let peer = peer_ip(&request);
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
    let Some(q) = query.get("q").filter(|q| !q.trim().is_empty()) else {
        send_json(request, 400, r#"{"error":"missing q parameter"}"#.into());
        return;
    };
    let limit: u32 = query
        .get("limit")
        .and_then(|l| l.parse().ok())
        .unwrap_or(30)
        .clamp(1, 100);
    let target = format!("ytsearch{limit}:{q}");

    let mut items: Vec<TrackMeta> = Vec::new();
    let started = Instant::now();
    let result = listing::list(
        Kind::Search,
        &target,
        limit,
        server_cookies(ctx),
        &mut |m| items.push(m),
    );
    match result {
        Ok(n) => {
            eprintln!(
                "web search ok: {n} items for {q:?} ({} ms)",
                started.elapsed().as_millis()
            );
            send_json(request, 200, json!({ "v": 1, "items": items }).to_string());
        }
        Err(e) => {
            eprintln!("web search failed: {e}");
            send_json(request, 502, json!({ "error": e }).to_string());
        }
    }
}

pub fn liked_request(request: Request, ctx: &Ctx) {
    if request.method() != &Method::Get {
        send_text(request, 405, "method not allowed");
        return;
    }
    if !crate::authorized(&request, &ctx.token, ctx.allow_private) {
        send_json(request, 401, r#"{"error":"bad token"}"#.into());
        return;
    }
    let peer = peer_ip(&request);
    if !ctx
        .rate
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow(&peer)
    {
        send_json(request, 429, r#"{"error":"rate limited"}"#.into());
        return;
    }
    if ctx.cookies.is_none() {
        send_json(
            request,
            400,
            r#"{"error":"this resolver has no --cookies, so Liked Music is not available"}"#.into(),
        );
        return;
    }
    if let Some((at, items)) = ctx
        .listings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get("liked")
    {
        if at.elapsed() < LISTING_TTL {
            send_json(request, 200, json!({ "v": 1, "items": items }).to_string());
            return;
        }
    }
    let mut items: Vec<TrackMeta> = Vec::new();
    let started = Instant::now();
    let result = listing::list(
        Kind::LikedMusic,
        LIKED_MUSIC_URL,
        WEB_FETCH_LIMIT,
        server_cookies(ctx),
        &mut |m| items.push(m),
    );
    match result {
        Ok(_) => {
            eprintln!(
                "web liked ok: {} items ({} ms)",
                items.len(),
                started.elapsed().as_millis()
            );
            ctx.listings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert("liked".into(), (Instant::now(), items.clone()));
            send_json(request, 200, json!({ "v": 1, "items": items }).to_string());
        }
        Err(e) => {
            eprintln!("web liked failed: {e}");
            send_json(request, 502, json!({ "error": e }).to_string());
        }
    }
}

/// Shared checks for the playlist routes: auth, rate limit and cookies.
/// Returns the request back when it may go on.
fn allow_listing(request: Request, ctx: &Ctx) -> Option<Request> {
    if request.method() != &Method::Get {
        send_text(request, 405, "method not allowed");
        return None;
    }
    if !crate::authorized(&request, &ctx.token, ctx.allow_private) {
        send_json(request, 401, r#"{"error":"bad token"}"#.into());
        return None;
    }
    let peer = peer_ip(&request);
    if !ctx
        .rate
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow(&peer)
    {
        send_json(request, 429, r#"{"error":"rate limited"}"#.into());
        return None;
    }
    if ctx.cookies.is_none() {
        send_json(
            request,
            400,
            r#"{"error":"this resolver has no --cookies, so playlists are not available"}"#.into(),
        );
        return None;
    }
    Some(request)
}

/// Answers a listing from the shared cache, or runs yt-dlp once and caches it.
fn cached_listing(request: Request, ctx: &Ctx, key: &str, kind: Kind, target: &str) {
    if let Some((at, items)) = ctx
        .listings
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
    {
        if at.elapsed() < LISTING_TTL {
            send_json(request, 200, json!({ "v": 1, "items": items }).to_string());
            return;
        }
    }
    let mut items: Vec<TrackMeta> = Vec::new();
    let started = Instant::now();
    match listing::list(kind, target, WEB_FETCH_LIMIT, server_cookies(ctx), &mut |m| {
        items.push(m)
    }) {
        Ok(_) => {
            eprintln!(
                "web {} ok: {} items ({} ms)",
                listing::label(kind),
                items.len(),
                started.elapsed().as_millis()
            );
            ctx.listings
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key.to_string(), (Instant::now(), items.clone()));
            send_json(request, 200, json!({ "v": 1, "items": items }).to_string());
        }
        Err(e) => {
            eprintln!("web {} failed: {e}", listing::label(kind));
            send_json(request, 502, json!({ "error": e }).to_string());
        }
    }
}

/// The account's channel id, needed for its playlists page.
///
/// This mirrors the terminal's account lookup: yt-dlp follows the stored jar
/// and the watch-later or liked playlist reports its owner. Found once per
/// process, since the resolver is restarted when the cookies change.
fn account_channel_id(ctx: &Ctx) -> Result<String, String> {
    if let Some(id) = ctx.account.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        return Ok(id);
    }
    let Some((flag, value)) = server_cookies(ctx) else {
        return Err("this resolver has no --cookies, so playlists are not available".into());
    };
    let _lock = ytui_resolve::lock_cookies_for_run(Some((flag, value)))?;
    let before = ytui_resolve::jar_session_state(Some((flag, value)));
    let mut last = String::new();
    for url in [WATCH_LATER_URL, LIKED_MUSIC_URL] {
        let out = ytui_resolve::command()
            .args([
                "--flat-playlist",
                "--playlist-items",
                "1",
                "-J",
                "--no-warnings",
                flag,
                value,
                url,
            ])
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|_| "yt-dlp not found".to_string())?;
        if !out.status.success() {
            last = ytui_resolve::friendly(&String::from_utf8_lossy(&out.stderr));
            continue;
        }
        let v: serde_json::Value = match serde_json::from_slice(&out.stdout) {
            Ok(v) => v,
            Err(e) => {
                last = format!("bad yt-dlp json: {e}");
                continue;
            }
        };
        let id = v
            .get("channel_id")
            .and_then(|x| x.as_str())
            .or_else(|| v.get("uploader_id").and_then(|x| x.as_str()))
            .unwrap_or("");
        if !id.is_empty() {
            *ctx.account.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.to_string());
            return Ok(id.to_string());
        }
    }
    if before == Some(true)
        && ytui_resolve::jar_session_state(Some((flag, value))) == Some(false)
    {
        return Err(ytui_resolve::SIGNED_OUT.into());
    }
    Err(if last.is_empty() {
        "could not read the account: cookies are accepted but YouTube returned no owner".into()
    } else {
        last
    })
}

/// Playlists created by the signed-in account.
pub fn playlists_request(request: Request, ctx: &Ctx) {
    let Some(request) = allow_listing(request, ctx) else {
        return;
    };
    let target = match account_channel_id(ctx) {
        Ok(id) => format!("https://www.youtube.com/channel/{id}/playlists"),
        Err(e) => {
            send_json(request, 502, json!({ "error": e }).to_string());
            return;
        }
    };
    cached_listing(request, ctx, "playlists", Kind::Playlists, &target);
}

/// Tracks in one playlist. The id is validated so a request cannot turn the
/// server into an arbitrary yt-dlp proxy.
pub fn playlist_request(request: Request, url: &str, ctx: &Ctx) {
    let Some(request) = allow_listing(request, ctx) else {
        return;
    };
    let query = parse_query(url);
    let Some(id) = query
        .get("id")
        .filter(|id| {
            (2..=100).contains(&id.len())
                && id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        })
    else {
        send_json(request, 400, r#"{"error":"missing or bad playlist id"}"#.into());
        return;
    };
    let target = format!("https://www.youtube.com/playlist?list={id}");
    let key = format!("playlist:{id}");
    cached_listing(request, ctx, &key, Kind::PlaylistTracks, &target);
}

pub fn audio_request(request: Request, path: &str, ctx: &Ctx) {
    if request.method() != &Method::Get {
        send_text(request, 405, "method not allowed");
        return;
    }
    if !crate::authorized(&request, &ctx.token, ctx.allow_private) {
        send_json(request, 401, r#"{"error":"bad token"}"#.into());
        return;
    }
    let peer = peer_ip(&request);
    if !ctx
        .audio_rate
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .allow(&peer)
    {
        send_json(request, 429, r#"{"error":"rate limited"}"#.into());
        return;
    }
    let Some(id) = path.strip_prefix("/audio/").filter(|id| crate::is_video_id(id)) else {
        send_json(request, 400, r#"{"error":"bad audio id"}"#.into());
        return;
    };
    let id = id.to_string();
    // The client's own Range header, passed through: `<audio>` seeks by
    // asking for the bytes it needs next.
    let range = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Range"))
        .map(|h| h.value.as_str().to_string())
        .filter(|r| r.starts_with("bytes="));

    let track = match resolve_id(ctx, &id, false) {
        Ok(track) => track,
        Err(e) => {
            eprintln!("audio {id}: resolve failed: {e}");
            send_json(request, 502, json!({ "error": e }).to_string());
            return;
        }
    };
    // Safari asks for the tail (`bytes=-N`) to find the moov atom of a long
    // mix. googlevideo answers only start ranges, so turn the suffix into an
    // explicit one before opening.
    let range = translate_suffix(&track, range.as_deref());
    if let Some(up) = open_upstream(&track, range.as_deref()) {
        respond_stream(request, up);
        return;
    }
    // Every candidate refused: the cached URLs may have expired. One fresh
    // resolve, then the same opening attempt again.
    if let Ok(track) = resolve_id(ctx, &id, true) {
        let range = translate_suffix(&track, range.as_deref());
        if let Some(up) = open_upstream(&track, range.as_deref()) {
            respond_stream(request, up);
            return;
        }
    }
    eprintln!("audio {id}: every candidate refused");
    send_json(request, 502, r#"{"error":"YouTube refused every AAC stream"}"#.into());
}

/// The candidates for a video, from the resolve cache unless `refresh`.
///
/// A cold resolve takes a gate slot, like `/v1/resolve` does, so the number
/// of yt-dlp children still follows `--max-concurrency`.
fn resolve_id(ctx: &Ctx, id: &str, refresh: bool) -> Result<Resolved, String> {
    if !refresh {
        if let Some(track) = ctx.cache.lock().unwrap_or_else(|e| e.into_inner()).get(id) {
            return Ok(track);
        }
    }
    let canonical = format!("https://www.youtube.com/watch?v={id}");
    let permit = ctx.gate.acquire();
    let started = Instant::now();
    let result = ytui_resolve::resolve(&canonical, server_cookies(ctx));
    drop(permit);
    let ms = started.elapsed().as_millis() as u64;
    match result {
        Ok(track) => {
            ctx.cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .put(id, track.clone());
            ctx.speed.lock().unwrap_or_else(|e| e.into_inner()).record(ms);
            eprintln!(
                "resolve {id} ok ({ms} ms, {} candidates)",
                track.candidates.len()
            );
            Ok(track)
        }
        Err(e) => Err(e),
    }
}

/// One upstream answer, ready to stream to the client.
struct Upstream {
    status: u16,
    content_type: Option<String>,
    content_range: Option<String>,
    accept_ranges: Option<String>,
    /// The body's byte count, so tiny_http frames the answer with
    /// Content-Length instead of closing the connection per chunk.
    content_length: Option<usize>,
    reader: Box<dyn Read + Send + Sync + 'static>,
}

/// Turns `bytes=-N` into `bytes=<size-N>-` using a one-byte range answer, so
/// a trailing-moov file can be read from the end. Anything else passes
/// through unchanged; if no candidate reports its size, the original range is
/// kept and fails the old way.
fn translate_suffix(track: &Resolved, range: Option<&str>) -> Option<String> {
    let Some(r) = range else { return None };
    let Some(n) = r
        .strip_prefix("bytes=-")
        .and_then(|n| n.parse::<u64>().ok())
    else {
        return Some(r.to_string());
    };
    track
        .candidates
        .iter()
        .take(MAX_TRIES)
        .find_map(|c| match c.size {
            Some(size) => Some(size.saturating_sub(n)),
            None => suffix_start(&c.url, n),
        })
        .map(|start| format!("bytes={start}-"))
        .or_else(|| Some(r.to_string()))
}

/// Start offset for the last `n` bytes of a candidate, read from the
/// Content-Range total of a one-byte range request.
fn suffix_start(candidate: &str, n: u64) -> Option<u64> {
    let resp = stream_agent()
        .get(candidate)
        .set("User-Agent", UA)
        .set("Range", "bytes=0-0")
        .call()
        .ok()?;
    let size: u64 = resp.header("Content-Range")?.rsplit('/').next()?.parse().ok()?;
    Some(size.saturating_sub(n))
}

/// Opens the ranged request on the first candidate that answers.
///
/// The resolve cache already ordered them best-first. Opening is itself the
/// probe — an unwanted URL answers 403 immediately — because this server is
/// also the machine that will fetch the stream.
fn open_upstream(track: &Resolved, range: Option<&str>) -> Option<Upstream> {
    let agent = stream_agent();
    for candidate in track.candidates.iter().take(MAX_TRIES) {
        let mut req = agent
            .get(&candidate.url)
            .set("User-Agent", UA);
        if let Some(range) = range {
            req = req.set("Range", range);
        }
        let Ok(resp) = req.call() else { continue };
        let status = resp.status();
        if status != 206 && status != 200 {
            continue;
        }
        return Some(Upstream {
            status,
            content_type: resp.header("Content-Type").map(String::from),
            content_range: resp.header("Content-Range").map(String::from),
            accept_ranges: resp.header("Accept-Ranges").map(String::from),
            content_length: resp.header("Content-Length").and_then(|v| v.parse().ok()),
            reader: resp.into_reader(),
        });
    }
    None
}

/// Passes the upstream answer through: same status, the range headers
/// verbatim, and the body streamed without buffering.
fn respond_stream(request: Request, up: Upstream) {
    let mut headers = Vec::new();
    let mut push = |name: &str, value: &str| {
        if let Ok(h) = Header::from_bytes(name.as_bytes(), value.as_bytes()) {
            headers.push(h);
        }
    };
    if let Some(ct) = &up.content_type {
        push("Content-Type", ct);
    }
    if let Some(cr) = &up.content_range {
        push("Content-Range", cr);
    }
    if let Some(ar) = &up.accept_ranges {
        push("Accept-Ranges", ar);
    }
    let response = Response::new(StatusCode(up.status), headers, up.reader, up.content_length, None);
    // The client may hang up mid-stream: a seek, a locked phone. The browser
    // retries with a new range request, so that is not an error to log.
    let _ = request.respond(response);
}

fn peer_ip(request: &Request) -> String {
    request
        .remote_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|| "local".into())
}

fn server_cookies(ctx: &Ctx) -> Option<(&str, &str)> {
    ctx.cookies
        .as_deref()
        .and_then(std::path::Path::to_str)
        .map(|p| ("--cookies", p))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// With no candidate to ask, a suffix range must reach upstream unchanged:
    /// the browser still gets its answer, and nothing is invented.
    #[test]
    fn a_suffix_range_falls_back_when_no_candidate_answers() {
        let track = Resolved {
            id: "x".into(),
            title: String::new(),
            channel: String::new(),
            duration: 0.0,
            candidates: Vec::new(),
            cached: false,
        };
        assert_eq!(
            translate_suffix(&track, Some("bytes=-1024")).as_deref(),
            Some("bytes=-1024")
        );
        assert_eq!(
            translate_suffix(&track, Some("bytes=0-1")).as_deref(),
            Some("bytes=0-1")
        );
        assert_eq!(translate_suffix(&track, None), None);
    }

    #[test]
    fn a_known_size_answers_a_tail_range_without_probing() {
        let track = Resolved {
            id: "x".into(),
            title: String::new(),
            channel: String::new(),
            duration: 0.0,
            candidates: vec![ytui_resolve::Candidate {
                url: "https://example.invalid/a".into(),
                itag: "140".into(),
                abr: 128,
                size: Some(1000),
            }],
            cached: false,
        };
        assert_eq!(
            translate_suffix(&track, Some("bytes=-100")).as_deref(),
            Some("bytes=900-")
        );
    }
}
