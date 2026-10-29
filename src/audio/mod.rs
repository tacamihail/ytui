//! Playback engine. One worker thread owns the decoder, queue and sink; the UI
//! sends `Cmd` messages and reads `State` behind a mutex, so drawing never
//! waits on the network or the audio device.

mod decode;
mod http;
mod resample;
mod sink;

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::ytdl;

use decode::Loaded;
use sink::Sink;

/// Fallback output rate. Tracks normally play at their own rate (44.1 kHz for
/// YouTube AAC); this is used when the device refuses that rate.
pub const OUT_RATE: u32 = 48_000;

/// How many dead tracks in a row playback walks past before it gives up.
const MAX_SKIPS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Stopped,
    Loading,
    Playing,
    Paused,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repeat {
    Off,
    All,
    One,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub title: String,
    pub channel: String,
    pub url: String,
    pub duration: u64,
}

#[derive(Debug, Clone)]
pub struct State {
    pub status: Status,
    pub title: String,
    pub channel: String,
    /// Watch URL of the current track, so the lists can mark its row.
    pub url: String,
    pub pos: f64,
    pub dur: f64,
    pub volume: i64,
    pub muted: bool,
    pub queue: Vec<Track>,
    pub index: usize,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub err: Option<String>,
    /// Set after a track is skipped because it would not resolve; the UI
    /// shows it once as a toast.
    pub notice: Option<String>,
    pub notice_seq: u64,
    /// Bumped whenever the queue contents change, so the UI can cache rows.
    pub version: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            status: Status::Stopped,
            title: String::new(),
            channel: String::new(),
            url: String::new(),
            pos: 0.0,
            dur: 0.0,
            volume: 100,
            muted: false,
            queue: Vec::new(),
            index: 0,
            shuffle: false,
            repeat: Repeat::Off,
            err: None,
            notice: None,
            notice_seq: 0,
            version: 1,
        }
    }
}

pub enum Cmd {
    /// Replace the queue with `tracks` and start at `index`.
    Play(Vec<Track>, usize),
    Append(Track),
    Toggle,
    Seek(f64),
    SeekAbs(f64),
    Volume(i64),
    Mute(bool),
    Next,
    Prev,
    PlayIndex(usize),
    RemoveIndex(usize),
    Stop,
    Shuffle(bool),
    Repeat(Repeat),
}

pub struct Player {
    tx: Sender<Cmd>,
    pub state: Arc<Mutex<State>>,
}

impl Player {
    pub fn new(cfg: &Config) -> Self {
        let state = Arc::new(Mutex::new(State::default()));
        let (tx, rx) = mpsc::channel();
        let engine_state = Arc::clone(&state);
        let cfg = cfg.clone();
        thread::spawn(move || Engine::new(cfg, engine_state).run(rx));
        Self { tx, state }
    }

    pub fn tx_clone(&self) -> Sender<Cmd> {
        self.tx.clone()
    }
}

struct Engine {
    cfg: Config,
    state: Arc<Mutex<State>>,
    sink: Option<Sink>,
    loaded: Option<Loaded>,
    eof: bool,
    /// Dead tracks walked past since the last time playback started on
    /// purpose; see `MAX_SKIPS`.
    fails: usize,
    /// Where the output should resume once the ring has enough audio, in
    /// seconds. `Some` while the sink is held after a start or a seek.
    holding: Option<f64>,
    /// Resolves the next track while the current one plays. `yt-dlp` takes a
    /// few seconds per track, which is most of the pause between songs.
    prefetch: Arc<Prefetch>,
    tmp: Vec<f32>,
    rng: u64,
}

/// Works out stream URLs in the background, one at a time, and keeps a small
/// LRU of recently resolved links.
///
/// Each run starts `yt-dlp`, which in turn starts deno to solve YouTube's
/// JavaScript challenge; together they use ~135 MiB for a couple of seconds.
/// Running two at once doubles that, so the background queue is handled by a
/// single thread. A link by itself is only ~1.2 KiB, so caching a handful is
/// free and turns replays and skips back into instant starts.
struct Prefetch {
    inner: Mutex<Job>,
    cv: Condvar,
    cfg: Config,
}

/// Resolved links kept for replays. Sixteen googlevideo URLs are ~25 KiB.
const URL_CACHE_MAX: usize = 16;
/// Upcoming tracks to resolve while the current one plays.
const PREFETCH_AHEAD: usize = 3;
/// Stop using a link this long before YouTube's own expiry, so there is time
/// to re-resolve when a track is played from an old cache entry.
const URL_EXPIRY_MARGIN_SECS: u64 = 5 * 60;
/// Space between background resolves. The first one still starts immediately,
/// the way the old single-track prefetch did; the extra ones are not urgent
/// and should not compete with the decoder in one burst.
const PREFETCH_GAP: Duration = Duration::from_millis(1200);
/// Gap before retrying a failed background resolve.
const PREFETCH_RETRY_GAP: Duration = Duration::from_millis(1500);

#[derive(Default)]
struct Job {
    /// Tracks to resolve in the background, in play order.
    queue: VecDeque<String>,
    /// URL being resolved right now.
    running: Option<String>,
    /// Bumped whenever `request` replaces the queue, so a job can tell whether
    /// the position it was started for is still the one being listened to.
    generation: u64,
    /// Oldest first. A hit moves its entry to the back.
    cache: VecDeque<(String, ytdl::Stream)>,
}

impl Prefetch {
    fn spawn(cfg: Config) -> Arc<Self> {
        let p = Arc::new(Self {
            inner: Mutex::new(Job::default()),
            cv: Condvar::new(),
            cfg,
        });
        let worker = Arc::clone(&p);
        let _ = thread::Builder::new()
            .name("ytui-resolve".into())
            .stack_size(512 * 1024)
            .spawn(move || worker.run());
        p
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Job> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn run(&self) {
        loop {
            let (url, generation) = {
                let mut g = self.lock();
                loop {
                    if let Some(u) = g.queue.pop_front() {
                        // A foreground click may have resolved it while it
                        // sat in the queue.
                        if g.cache.iter().any(|(c, _)| c == &u) {
                            continue;
                        }
                        g.running = Some(u.clone());
                        break (u, g.generation);
                    }
                    g = self.cv.wait(g).unwrap_or_else(|e| e.into_inner());
                }
            };

            let mut res = ytdl::stream_url(&url, &self.cfg);
            if let Err(e) = &res {
                // A transient 403 or a killed yt-dlp should not leave the next
                // handover to resolve the track on the audio thread, which is
                // exactly where a pause between songs comes from. A timeout is
                // not retried, and a jump that replaced the queue means this
                // offset is no longer the one being listened to.
                if !e.contains("timed out") && self.lock().generation == generation {
                    thread::sleep(PREFETCH_RETRY_GAP);
                    res = ytdl::stream_url(&url, &self.cfg);
                }
            }

            let mut g = self.lock();
            g.running = None;
            if let Ok(s) = res {
                remember(&mut g, &url, s);
            }
            drop(g);
            self.cv.notify_all();
            // Resolving three tracks back to back would keep one core busy
            // for several seconds right after playback starts. One job is
            // enough of a head start; give the decoder a clear run before the
            // next, so the extra prefetch cannot cause underruns.
            thread::sleep(PREFETCH_GAP);
        }
    }

    /// Replace the background queue with the tracks after the one that just
    /// started. Replacing instead of appending matters after a jump: guesses
    /// made for the old position would otherwise sit in front of the list for
    /// the new one.
    fn request(&self, urls: Vec<String>) {
        let mut g = self.lock();
        g.generation = g.generation.wrapping_add(1);
        let mut queue = VecDeque::new();
        for url in urls {
            let known = g.cache.iter().any(|(u, _)| u == &url)
                || g.running.as_deref() == Some(url.as_str())
                || queue.contains(&url);
            if !known {
                queue.push_back(url);
            }
        }
        while queue.len() > PREFETCH_AHEAD {
            queue.pop_back();
        }
        let changed = g.queue != queue;
        g.queue = queue;
        drop(g);
        if changed {
            self.cv.notify_all();
        }
    }

    /// A cached, still-valid link for `url`.
    fn take(&self, url: &str) -> Option<ytdl::Stream> {
        take_cached(&mut self.lock(), url)
    }

    fn running(&self, url: &str) -> bool {
        self.lock().running.as_deref() == Some(url)
    }

    fn drop_queued(&self, url: &str) {
        self.lock().queue.retain(|u| u != url);
    }

    fn remember(&self, url: &str, stream: &ytdl::Stream) {
        remember(&mut self.lock(), url, stream.clone());
    }

    /// Waits for a resolve that is already in flight for exactly `url`. `None`
    /// means it finished without a usable link and the caller should resolve
    /// the track itself.
    fn wait(&self, url: &str, limit: Duration) -> Option<ytdl::Stream> {
        let deadline = Instant::now() + limit;
        let mut g = self.lock();
        loop {
            if let Some(s) = take_cached(&mut g, url) {
                return Some(s);
            }
            if g.running.as_deref() != Some(url) {
                return None;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            let (next, _) = self
                .cv
                .wait_timeout(g, left)
                .unwrap_or_else(|e| e.into_inner());
            g = next;
        }
    }
}

fn cached_index(g: &Job, url: &str) -> Option<usize> {
    g.cache.iter().position(|(u, _)| u == url)
}

/// Removes a cache hit and puts it back at the back, unless YouTube's expiry
/// timestamp says the link is about to stop working.
fn take_cached(g: &mut Job, url: &str) -> Option<ytdl::Stream> {
    let i = cached_index(g, url)?;
    let (u, s) = g.cache.remove(i)?;
    if link_expired(&s.url) {
        return None;
    }
    g.cache.push_back((u, s.clone()));
    Some(s)
}

fn remember(g: &mut Job, url: &str, s: ytdl::Stream) {
    if let Some(i) = cached_index(g, url) {
        g.cache.remove(i);
    }
    g.cache.push_back((url.to_string(), s));
    while g.cache.len() > URL_CACHE_MAX {
        g.cache.pop_front();
    }
}

/// googlevideo URLs carry `expire=<unix seconds>`. A link without the
/// parameter is assumed usable; the decoder's own 403 handling still applies.
fn link_expired(url: &str) -> bool {
    let Some((_, query)) = url.split_once('?') else {
        return false;
    };
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("expire=") {
            if let Ok(exp) = v.parse::<u64>() {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                return exp <= now + URL_EXPIRY_MARGIN_SECS;
            }
        }
    }
    false
}

impl Engine {
    fn new(cfg: Config, state: Arc<Mutex<State>>) -> Self {
        let prefetch = Prefetch::spawn(cfg.clone());
        let mut engine = Self {
            cfg,
            state,
            sink: None,
            loaded: None,
            eof: false,
            fails: 0,
            holding: None,
            prefetch,
            tmp: Vec::with_capacity(16384),
            rng: seed(),
        };
        match Sink::new(
            sink::Sink::default_rate().unwrap_or(OUT_RATE),
            engine.cfg.buffer_ms,
        ) {
            Ok(s) => {
                {
                    // Start quiet: a TUI player has no reason to blast the
                    // first track at full scale.
                    let mut st = engine.lock();
                    st.volume = engine.cfg.volume.clamp(0, 100);
                }
                s.set_volume(engine.lock().volume);
                engine.sink = Some(s);
            }
            Err(e) => {
                let mut st = engine.lock();
                st.status = Status::Error;
                st.err = Some(e);
            }
        }
        engine
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(12)) {
                Ok(cmd) => self.handle(cmd),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            self.tick();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn sink_rate(&self) -> u32 {
        self.sink.as_ref().map(|s| s.rate()).unwrap_or(OUT_RATE)
    }

    fn target_frames(&self) -> usize {
        (self.sink_rate() as usize * self.cfg.buffer_ms as usize / 1000).max(4096)
    }

    /// Stop the output and remember where it should resume. The decoder fills
    /// the ring from there, and `tick` lets it go once there is enough audio.
    /// The silence in between is not counted, so the clock starts where the
    /// sound does instead of a few seconds in.
    fn hold_at(&mut self, at: f64) {
        self.holding = Some(at);
        if let Some(s) = &self.sink {
            s.clear();
            s.set_played_frames((at * f64::from(s.rate())) as u64);
            s.set_hold(true);
        }
        self.lock().pos = at;
    }

    /// The stream for `url`: from the cache when it is a replay or a skip
    /// back, from the background thread when that is already resolving this
    /// exact URL, otherwise straight from yt-dlp. A click never waits behind
    /// the prefetch queue.
    fn resolve(&self, url: &str) -> Result<ytdl::Stream, String> {
        if let Some(s) = self.prefetch.take(url) {
            return Ok(s);
        }
        // This is the usual case when a song is skipped early: waiting for the
        // resolver that is already on this URL is faster than starting a
        // second one, and it keeps only one yt-dlp alive.
        if self.prefetch.running(url) {
            if let Some(s) = self.prefetch.wait(url, Duration::from_secs(20)) {
                return Ok(s);
            }
        }
        // The requested track is not the one being prefetched: resolve it on
        // this thread so the user hears it now, even if that means two yt-dlp
        // processes overlap for a moment.
        self.prefetch.drop_queued(url);
        let res = ytdl::stream_url(url, &self.cfg);
        if let Ok(stream) = &res {
            self.prefetch.remember(url, stream);
        }
        res
    }

    /// Ask for the next few tracks' URLs once the current one is known to
    /// play. The worker runs them one at a time; the queue is only as deep as
    /// a few skips so links do not expire before they are used.
    fn prefetch_after(&self, index: usize) {
        // Escape hatch for slow machines (or for A/B testing a stutter):
        // `YTUI_PREFETCH=0 ytui` resolves every track on demand only.
        if std::env::var("YTUI_PREFETCH").ok().as_deref() == Some("0") {
            return;
        }
        let urls = {
            let st = self.lock();
            let len = st.queue.len();
            let mut urls = Vec::new();
            if st.repeat == Repeat::One {
                if let Some(t) = st.queue.get(index) {
                    urls.push(t.url.clone());
                }
            } else if !st.shuffle {
                for step in 1..=PREFETCH_AHEAD {
                    let next = index + step;
                    let next = if next < len {
                        Some(next)
                    } else if st.repeat == Repeat::All && len > 0 {
                        Some(next % len)
                    } else {
                        None
                    };
                    if let Some(t) = next.and_then(|i| st.queue.get(i)) {
                        urls.push(t.url.clone());
                    }
                }
            }
            urls
        };
        if !urls.is_empty() {
            self.prefetch.request(urls);
        }
    }

    /// Reopens the output stream at `want` when the track's rate differs from
    /// the current one. If the device rejects it, the old stream stays and the
    /// decoder resamples to its rate.
    fn retune_sink(&mut self, want: u32) {
        if self.sink.as_ref().map(|s| s.rate()) == Some(want) {
            return;
        }
        let (vol, muted) = {
            let st = self.lock();
            (st.volume, st.muted)
        };
        if let Ok(s) = Sink::new(want, self.cfg.buffer_ms) {
            s.set_volume(vol);
            s.set_muted(muted);
            self.sink = Some(s);
        }
    }

    fn handle(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Play(tracks, index) => {
                let index = index.min(tracks.len().saturating_sub(1));
                {
                    let mut st = self.lock();
                    st.queue = tracks;
                    st.version = st.version.wrapping_add(1);
                    st.err = None;
                }
                self.fails = 0;
                self.start(index);
            }
            Cmd::Append(track) => {
                let (index, new_is_next) = {
                    let mut st = self.lock();
                    st.queue.push(track);
                    st.version = st.version.wrapping_add(1);
                    let len = st.queue.len();
                    (st.index, len >= 2 && st.index + 1 == len - 1)
                };
                if new_is_next {
                    self.prefetch_after(index);
                }
            }
            Cmd::Toggle => {
                let (status, index) = {
                    let st = self.lock();
                    (st.status, st.index)
                };
                if status == Status::Error {
                    // Space on a failed track is the natural "try again".
                    self.fails = 0;
                    self.start(index);
                } else if let Some(s) = &self.sink {
                    let paused = !s.is_paused();
                    s.set_paused(paused);
                    let mut st = self.lock();
                    if matches!(st.status, Status::Playing | Status::Paused) {
                        st.status = if paused {
                            Status::Paused
                        } else {
                            Status::Playing
                        };
                    }
                }
            }
            Cmd::Seek(delta) => {
                let pos = self.sink.as_ref().map(|s| s.played_frames()).unwrap_or(0) as f64
                    / f64::from(self.sink_rate());
                self.seek_to(pos + delta);
            }
            Cmd::SeekAbs(secs) => self.seek_to(secs),
            Cmd::Volume(v) => {
                let v = v.clamp(0, 100);
                if let Some(s) = &self.sink {
                    s.set_volume(v);
                }
                let mut st = self.lock();
                st.volume = v;
            }
            Cmd::Mute(m) => {
                if let Some(s) = &self.sink {
                    s.set_muted(m);
                }
                let mut st = self.lock();
                st.muted = m;
            }
            Cmd::Next => {
                self.fails = 0;
                self.skip(1);
            }
            Cmd::Prev => {
                self.fails = 0;
                self.skip(-1);
            }
            Cmd::PlayIndex(i) => {
                let len = self.lock().queue.len();
                if i < len {
                    self.fails = 0;
                    self.start(i);
                }
            }
            Cmd::RemoveIndex(i) => {
                let mut st = self.lock();
                if i < st.queue.len() {
                    st.queue.remove(i);
                    if i < st.index {
                        st.index -= 1;
                    }
                    st.version = st.version.wrapping_add(1);
                }
            }
            Cmd::Stop => {
                self.loaded = None;
                self.eof = false;
                self.holding = None;
                if let Some(s) = &self.sink {
                    s.clear();
                    s.set_played_frames(0);
                    s.set_paused(false);
                    s.set_hold(false);
                }
                let mut st = self.lock();
                st.status = Status::Stopped;
                st.pos = 0.0;
                st.dur = 0.0;
                st.title.clear();
                st.channel.clear();
                st.url.clear();
            }
            Cmd::Shuffle(on) => {
                let mut st = self.lock();
                st.shuffle = on;
            }
            Cmd::Repeat(mode) => {
                let mut st = self.lock();
                st.repeat = mode;
            }
        }
    }

    fn seek_to(&mut self, secs: f64) {
        let Some(loaded) = &mut self.loaded else {
            return;
        };
        let secs = secs.max(0.0);
        let Ok(actual) = loaded.seek(secs) else {
            return;
        };
        self.eof = false;
        self.hold_at(actual);
    }

    fn skip(&mut self, dir: i32) {
        let (len, index) = {
            let st = self.lock();
            (st.queue.len(), st.index)
        };
        if len == 0 {
            return;
        }
        let next = if dir > 0 {
            (index + 1) % len
        } else {
            (index + len - 1) % len
        };
        self.start(next);
    }

    fn start(&mut self, index: usize) {
        let track = {
            let st = self.lock();
            st.queue.get(index).cloned()
        };
        let Some(track) = track else {
            return;
        };

        self.loaded = None;
        self.eof = false;
        if let Some(s) = &self.sink {
            s.set_paused(false);
        }
        self.hold_at(0.0);
        {
            let mut st = self.lock();
            st.index = index;
            st.status = Status::Loading;
            st.title = track.title.clone();
            st.channel = track.channel.clone();
            st.url = track.url.clone();
            st.pos = 0.0;
            st.dur = track.duration as f64;
            st.err = None;
        }

        let loaded = self.resolve(&track.url).and_then(|stream| {
            // Entries created from a bare URL start out with no title or
            // duration; yt-dlp just told us both, so adopt them.
            if track.title.is_empty() || track.duration == 0 {
                let mut st = self.lock();
                if st.title.is_empty() {
                    st.title = stream.title.clone();
                }
                if st.channel.is_empty() {
                    st.channel = stream.channel.clone();
                }
                if st.dur <= 0.0 && stream.duration > 0.0 {
                    st.dur = stream.duration;
                }
                if let Some(t) = st.queue.get_mut(index) {
                    if t.title.is_empty() {
                        t.title = stream.title.clone();
                    }
                    if t.channel.is_empty() {
                        t.channel = stream.channel.clone();
                    }
                    if t.duration == 0 {
                        t.duration = stream.duration.max(0.0) as u64;
                    }
                }
                st.version = st.version.wrapping_add(1);
            }
            Loaded::open(&stream.url, self.cfg.cache_kib as usize * 1024)
        });

        match loaded {
            Ok(mut l) => {
                self.fails = 0;
                // Play at the track's rate when the device takes it; this
                // avoids resampling 44.1 kHz AAC up to 48 kHz and back.
                self.retune_sink(l.rate());
                l.set_target_rate(self.sink_rate());
                // A rate change replaces the sink, which starts out released.
                self.hold_at(0.0);

                let mut st = self.lock();
                st.status = if st.status == Status::Paused {
                    Status::Paused
                } else {
                    Status::Playing
                };
                if l.duration > 0.0 {
                    st.dur = l.duration;
                }
                drop(st);
                self.loaded = Some(l);
                self.prefetch_after(index);
            }
            Err(e) => {
                // One dead upload should not end the evening. Walk past
                // tracks YouTube refuses to serve, but stop short of
                // hammering through a whole list.
                let len = self.lock().queue.len();
                if len > 1 && self.fails + 1 < MAX_SKIPS && track_is_gone(&e) {
                    self.fails += 1;
                    let mut st = self.lock();
                    st.notice = Some(format!("{e}, skipped {}", st.title));
                    st.notice_seq = st.notice_seq.wrapping_add(1);
                    drop(st);
                    self.start((index + 1) % len);
                    return;
                }
                let mut st = self.lock();
                st.status = Status::Error;
                st.err = Some(if self.fails > 0 {
                    format!("{} tracks in a row would not play: {e}", self.fails + 1)
                } else {
                    e
                });
            }
        }
    }

    fn fail(&mut self, err: String) {
        self.loaded = None;
        self.eof = false;
        self.holding = None;
        if let Some(s) = &self.sink {
            s.clear();
            s.set_hold(false);
        }
        let mut st = self.lock();
        st.status = Status::Error;
        st.err = Some(err);
    }

    fn tick(&mut self) {
        if let Some(s) = &self.sink {
            if let Some(e) = s.take_error() {
                self.fail(e);
                return;
            }
        }

        // Pump the decoder while the ring buffer has room. After a start or a
        // seek only a short pre-roll is needed to let go of the output, so do
        // not sit there decoding the whole buffer first.
        if self.loaded.is_some() && !self.eof {
            loop {
                let Some(sink) = &self.sink else { break };
                let limit = if self.holding.is_some() {
                    (sink.rate() as usize / 2).max(1)
                } else {
                    self.target_frames()
                };
                if sink.buffered_frames() >= limit {
                    break;
                }
                let mut tmp = std::mem::take(&mut self.tmp);
                tmp.clear();
                let res = match &mut self.loaded {
                    Some(l) => l.next(&mut tmp),
                    None => Ok(false),
                };
                let samples = std::mem::take(&mut tmp);
                match res {
                    Ok(true) => {
                        if let Some(sink) = &self.sink {
                            sink.push(&samples);
                        }
                        self.tmp = samples;
                    }
                    Ok(false) => {
                        self.eof = true;
                        self.tmp = samples;
                        break;
                    }
                    Err(e) => {
                        self.tmp = samples;
                        self.fail(e);
                        break;
                    }
                }
            }
        }

        // Let the output go once there is enough audio to play without a gap.
        if let Some(at) = self.holding {
            if let Some(s) = &self.sink {
                let need = (s.rate() as usize / 2).max(1);
                if s.buffered_frames() >= need || self.eof {
                    s.set_played_frames((at * f64::from(s.rate())) as u64);
                    s.set_hold(false);
                    self.holding = None;
                }
            }
        }

        // Track finished?
        let drained = self
            .sink
            .as_ref()
            .map(|s| s.buffered_frames() == 0)
            .unwrap_or(true);
        if self.eof && drained {
            self.advance();
            return;
        }

        if let Some(s) = &self.sink {
            let pos = s.played_frames() as f64 / f64::from(s.rate());
            let paused = s.is_paused();
            let mut st = self.lock();
            if matches!(st.status, Status::Playing | Status::Paused) {
                st.pos = pos;
                st.status = if paused {
                    Status::Paused
                } else {
                    Status::Playing
                };
            }
        }
    }

    fn advance(&mut self) {
        let (index, len, repeat, shuffle) = {
            let st = self.lock();
            (st.index, st.queue.len(), st.repeat, st.shuffle)
        };

        // Repeat-one on the same stream: rewind instead of paying for another
        // yt-dlp resolution.
        if repeat == Repeat::One && len > 0 {
            if let Some(loaded) = &mut self.loaded {
                if loaded.seek(0.0).is_ok() {
                    self.eof = false;
                    self.hold_at(0.0);
                    let mut st = self.lock();
                    st.status = Status::Playing;
                    return;
                }
            }
        }

        self.eof = false;
        self.loaded = None;
        self.holding = None;
        if let Some(s) = &self.sink {
            s.clear();
            s.set_played_frames(0);
            s.set_hold(false);
        }

        if len == 0 {
            return;
        }

        let next = match repeat {
            Repeat::One => Some(index),
            _ if shuffle => Some(self.rand_index(len, index)),
            _ if index + 1 < len => Some(index + 1),
            Repeat::All => Some(0),
            Repeat::Off => None,
        };

        match next {
            Some(i) => self.start(i),
            None => {
                let mut st = self.lock();
                st.status = Status::Stopped;
                st.pos = 0.0;
                st.title.clear();
                st.channel.clear();
                st.url.clear();
            }
        }
    }

    fn rand_index(&mut self, len: usize, current: usize) -> usize {
        if len <= 1 {
            return current;
        }
        // xorshift64*: good enough for shuffle, no dependency.
        let mut x = self.rng | 1;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        let r = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as usize;
        let pick = r % (len - 1);
        if pick >= current {
            pick + 1
        } else {
            pick
        }
    }
}

/// Preferred rate of the default output device, for the memory estimate.
pub fn output_rate() -> u32 {
    sink::Sink::default_rate().unwrap_or(OUT_RATE)
}

/// True when the failure means "this track is gone", as opposed to a broken
/// setup (cookies, old yt-dlp, missing network) that would fail every track.
fn track_is_gone(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("unavailable")
        || e.contains("no aac stream")
        || e.contains("private")
        || e.contains("members-only")
        || e.contains("age-restricted")
}

/// Opens the output device for a moment; used by `ytui doctor`.
///
/// This goes through the same fallback as playback, so a WSL setup reports
/// `pulse @ 44100 Hz` instead of the card 0 failure ALSA's `default` hits.
pub fn check_device() -> Result<String, String> {
    let rate = sink::Sink::default_rate().unwrap_or(OUT_RATE);
    let opened = sink::Sink::new(rate, 200)?;
    Ok(format!(
        "{} @ {} Hz stereo",
        opened.device_name(),
        opened.rate()
    ))
}

fn seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    nanos ^ (std::process::id() as u64) << 32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_hopeless_tracks_are_skipped() {
        // What `ytdl::friendly` hands over for a removed upload.
        assert!(track_is_gone("unavailable on youtube"));
        assert!(track_is_gone("no AAC stream for this track (opus only)"));
        assert!(track_is_gone(
            "private: sign-in required, run `ytui auth <cookies.txt>`"
        ));

        // Setup problems: skipping would just burn through the queue.
        assert!(!track_is_gone(
            "cookies error, re-export cookies.txt (ERROR: cookies are no longer valid)"
        ));
        assert!(!track_is_gone("yt-dlp is too old, update it"));
        assert!(!track_is_gone(
            "yt-dlp not found, install it and run: ytui doctor"
        ));
        assert!(!track_is_gone("open stream: connection refused"));
        assert!(!track_is_gone("cannot open audio device: no such device"));
    }

    fn stream(url: &str) -> ytdl::Stream {
        ytdl::Stream {
            url: url.to_string(),
            title: "track".into(),
            channel: "channel".into(),
            duration: 180.0,
        }
    }

    fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    fn link(expire: u64) -> String {
        format!("https://rr1.googlevideo.com/videoplayback?expire={expire}&mime=audio")
    }

    #[test]
    fn expired_links_are_not_reused() {
        assert!(link_expired(&link(now().saturating_sub(1))));
        // Expiring within the safety margin counts as expired, so there is
        // time to resolve a fresh link before playback starts.
        assert!(link_expired(&link(now() + 60)));
        assert!(!link_expired(&link(now() + 3600)));
        // No `expire=` parameter: assume usable and let the decoder decide.
        assert!(!link_expired("https://rr1.googlevideo.com/videoplayback?mime=audio"));
    }

    #[test]
    fn the_url_cache_keeps_the_newest_entries() {
        let mut g = Job::default();
        for i in 0..(URL_CACHE_MAX + 4) {
            remember(&mut g, &format!("u{i}"), stream(&link(now() + 3600)));
        }
        assert_eq!(g.cache.len(), URL_CACHE_MAX);
        assert!(take_cached(&mut g, "u0").is_none());
        let newest = format!("u{}", URL_CACHE_MAX + 3);
        assert!(take_cached(&mut g, &newest).is_some());
        // A hit stays cached, so skipping back is instant too.
        assert!(take_cached(&mut g, &newest).is_some());
    }

    #[test]
    fn a_jump_replaces_the_prefetch_queue() {
        let p = Prefetch {
            inner: Mutex::new(Job::default()),
            cv: Condvar::new(),
            cfg: Config::default(),
        };
        p.request(vec!["a".into(), "b".into(), "c".into()]);
        assert_eq!(
            p.lock().queue,
            VecDeque::from(vec!["a".into(), "b".into(), "c".into()])
        );
        // Jumping elsewhere must not leave the old position's guesses in front.
        p.request(vec!["x".into()]);
        assert_eq!(p.lock().queue, VecDeque::from(vec!["x".into()]));
        // A track that is already resolved is not queued again.
        remember(&mut p.lock(), "x", stream(&link(now() + 3600)));
        p.request(vec!["x".into(), "y".into()]);
        assert_eq!(p.lock().queue, VecDeque::from(vec!["y".into()]));
    }
}
