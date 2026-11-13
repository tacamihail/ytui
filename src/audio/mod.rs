//! Playback engine. One worker thread owns the decoder, queue and sink; the UI
//! sends `Cmd` messages and reads `State` behind a mutex, so drawing never
//! waits on the network or the audio device.

mod decode;
mod http;
mod resample;
mod sink;
mod tail;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
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
    /// What the footer shows while a track is being resolved or a seek is
    /// landing. `None` when nothing is in flight or the last one aged out.
    pub activity: Option<Activity>,
    /// Set after a track is skipped because it would not resolve; the UI
    /// shows it once as a toast.
    pub notice: Option<String>,
    /// Whether `notice` is an error (red) or just information (green).
    pub notice_err: bool,
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
            activity: None,
            notice: None,
            notice_err: false,
            notice_seq: 0,
            version: 1,
        }
    }
}

/// What the footer is doing right now, so a slow resolve or seek is visible
/// instead of looking like a hang. One slot: a newer event replaces it.
#[derive(Debug, Clone)]
pub struct Activity {
    pub kind: ActivityKind,
    /// Track title for a resolve, `m:ss` target for a seek.
    pub label: String,
    pub started: Instant,
    pub done: Option<Done>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKind {
    Resolve,
    Seek,
}

#[derive(Debug, Clone, Copy)]
pub struct Done {
    /// When it finished; the footer keeps the line for three seconds after.
    pub at: Instant,
    /// Time from the command to the sound.
    pub elapsed: Duration,
    /// Served from a cache instead of a fresh yt-dlp run. Shown as a marker,
    /// and shown *only* on a hit: a cold resolve stays silent.
    pub cached: bool,
}

const ACTIVITY_DONE_FOR: Duration = Duration::from_secs(3);

impl Activity {
    fn new(kind: ActivityKind, label: String) -> Self {
        Self {
            kind,
            label,
            started: Instant::now(),
            done: None,
        }
    }

    fn finish(&mut self, cached: bool) {
        if self.done.is_none() {
            self.done = Some(Done {
                at: Instant::now(),
                elapsed: self.started.elapsed(),
                cached,
            });
        }
    }

    /// The footer line, or `None` once a finished one has had its three
    /// seconds. Emoji are two cells wide in most terminals, so callers size
    /// the line by display width rather than by char count.
    pub fn line(&self) -> Option<String> {
        match &self.done {
            None => Some(format!("{} {}", self.pending_icon(), self.label)),
            Some(done) => {
                if done.at.elapsed() > ACTIVITY_DONE_FOR {
                    return None;
                }
                let mut line = format!("✅ {} · {}", self.label, seconds(done.elapsed));
                if done.cached {
                    line.push_str(" · 📦");
                }
                Some(line)
            }
        }
    }

    fn pending_icon(&self) -> &'static str {
        match self.kind {
            ActivityKind::Resolve => "⏳",
            ActivityKind::Seek => "⏩",
        }
    }
}

/// One decimal below ten seconds, whole seconds above: enough to tell a warm
/// resolve from a cold one without eating the footer.
fn seconds(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 10.0 {
        format!("{s:.1}s")
    } else {
        format!("{}s", s.round() as u64)
    }
}

fn mmss(secs: f64) -> String {
    let total = secs.max(0.0) as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

pub enum Cmd {
    /// Replace the queue with `tracks` and start at `index`.
    Play(Vec<Track>, usize),
    Append(Track),
    /// Start or resume without toggling: the OS play button must not pause a
    /// track that is already playing.
    Resume,
    /// Pause without toggling, for the OS pause button.
    Pause,
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
    /// Replace the resolver/audio config after the `?` menu found a resolver.
    SetConfig(Config),
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
    /// Last time the default output device was polled, and the throttle for
    /// reopening a stream whose device went away.
    device_check: Instant,
    last_rebuild: Instant,
    /// Resolves the next track in the last seconds of the current one.
    /// `yt-dlp` takes a few seconds per track, which is most of the pause
    /// between songs.
    prefetch: Arc<Prefetch>,
    /// URL of the next track already handed to the resolver for this song.
    /// Keyed by URL so a queue edit or a repeat-mode change still triggers a
    /// resolve for the new next track.
    prefetched: Option<String>,
    /// The foreground resolve, while it is running. `None` once its answer has
    /// been applied (or the selection was superseded).
    load: Option<Load>,
    /// Bumped for every foreground load, so a late answer for an older
    /// selection can be recognised and dropped.
    load_seq: u64,
    /// How long recent cold resolves took; drives the prefetch lead.
    resolve_ms: ResolveTimes,
    /// The tail scan, while it is running.
    tail: Option<TailScan>,
    /// Where this track's trailing silence starts, once known.
    tail_cut: Option<f64>,
    /// URL and seek generation of the last scan, so a track with no quiet
    /// tail is not scanned again on every tick.
    tail_done: Option<(String, u64)>,
    /// Bumped whenever the playhead jumps, so a verdict is not reused after a
    /// seek to a different position.
    seek_gen: u64,
    tmp: Vec<f32>,
    rng: u64,
}

/// A resolve running for the selection the listener just made, off the audio
/// thread. `tick` polls `rx`, so a slow yt-dlp or resolver cannot stall the
/// engine loop: the previous sound keeps playing and a newer ENTER wins
/// immediately instead of queueing behind this one.
struct Load {
    seq: u64,
    /// When the resolve started, for the footer and the adaptive lead.
    started: Instant,
    /// Raised when a newer selection takes over. The worker kills the yt-dlp
    /// child (and its deno) instead of running to completion.
    token: Arc<AtomicBool>,
    rx: Receiver<Result<ytdl::Stream, String>>,
}

/// A tail scan on its own thread; the answer is one number, or nothing.
struct TailScan {
    token: Arc<AtomicBool>,
    rx: Receiver<Option<f64>>,
}

/// Recent cold resolves, so the next track can be requested early enough for
/// the machine it is resolved on.
#[derive(Default)]
struct ResolveTimes {
    samples: VecDeque<Duration>,
}

impl ResolveTimes {
    fn record(&mut self, took: Duration) {
        if self.samples.len() == RESOLVE_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back(took);
    }

    fn median(&self) -> Option<Duration> {
        if self.samples.is_empty() {
            return None;
        }
        let mut sorted: Vec<Duration> = self.samples.iter().copied().collect();
        sorted.sort();
        Some(sorted[sorted.len() / 2])
    }
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
    cfg: Mutex<Config>,
}

/// Resolved links kept for replays. Sixteen googlevideo URLs are ~25 KiB.
const URL_CACHE_MAX: usize = 16;
/// Start resolving the next track this long before the current one ends at the
/// least. A cold resolver takes about twenty seconds; a warm one a fraction of
/// that, and this floor is what keeps the handover free of dead air.
const RESOLVE_LEAD_MIN_SECS: f64 = 20.0;
/// Extra room on top of twice the measured resolve time: enough for the
/// handover itself, and for the next track's own start-up.
const RESOLVE_LEAD_MARGIN_SECS: f64 = 5.0;
/// Resolve times kept for the median.
const RESOLVE_SAMPLES: usize = 8;
/// Stop using a link this long before YouTube's own expiry, so there is time
/// to re-resolve when a track is played from an old cache entry.
const URL_EXPIRY_MARGIN_SECS: u64 = 5 * 60;
/// Gap before retrying a failed background resolve.
const PREFETCH_RETRY_GAP: Duration = Duration::from_millis(1500);
/// How often the system default output device is checked while a stream is
/// open. Windows keeps an open WASAPI stream on the endpoint it was created
/// for, so this poll is what moves playback to headphones connected mid-song.
const DEVICE_POLL: Duration = Duration::from_secs(2);

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
            cfg: Mutex::new(cfg),
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

    fn set_config(&self, cfg: Config) {
        *self.cfg.lock().unwrap_or_else(|e| e.into_inner()) = cfg;
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

            let cfg = self.cfg.lock().unwrap_or_else(|e| e.into_inner()).clone();
            let mut res = ytdl::stream_url(&url, &cfg);
            if let Err(e) = &res {
                // A transient 403 or a killed yt-dlp should not leave the next
                // handover to resolve the track on the audio thread, which is
                // exactly where a pause between songs comes from. A timeout is
                // not retried, and a jump that replaced the queue means this
                // offset is no longer the one being listened to.
                if !e.contains("timed out") && self.lock().generation == generation {
                    thread::sleep(PREFETCH_RETRY_GAP);
                    res = ytdl::stream_url(&url, &cfg);
                }
            }

            let mut g = self.lock();
            g.running = None;
            if let Ok(s) = res {
                remember(&mut g, &url, s);
            }
            drop(g);
            self.cv.notify_all();
        }
    }

    /// Replace the background queue with `urls`. Replacing instead of
    /// appending matters after a jump: guesses made for the old position would
    /// otherwise sit in front of the list for the new one.
    fn request(&self, urls: Vec<String>) {
        let mut g = self.lock();
        g.generation = g.generation.wrapping_add(1);
        let mut queue = VecDeque::new();
        for url in urls {
            // An expired cache entry is not a reason to skip the resolve: it
            // would be dropped at the handover anyway.
            let known = g
                .cache
                .iter()
                .any(|(u, s)| u == &url && !link_expired(&s.url))
                || g.running.as_deref() == Some(url.as_str())
                || queue.contains(&url);
            if !known {
                queue.push_back(url);
            }
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

/// The foreground resolve, run off the audio thread.
///
/// A URL the background worker is already on is waited for instead of starting
/// a second yt-dlp: skipping early is the usual case, and two runs would double
/// the ~135 MiB yt-dlp + deno peak for no gain. Anything solved here is left in
/// the cache, so going back to it starts instantly.
fn load_stream(
    url: &str,
    cfg: &Config,
    prefetch: &Prefetch,
    cancel: &AtomicBool,
) -> Result<ytdl::Stream, String> {
    if cancel.load(Ordering::Relaxed) {
        return Err(ytdl::CANCELLED.into());
    }
    if let Some(mut stream) = prefetch.take(url) {
        // Coming from the LRU is exactly the case the footer marks.
        stream.cached = true;
        return Ok(stream);
    }
    let waited = prefetch.running(url).then(|| prefetch.wait(url, Duration::from_secs(20)));
    let res = match waited {
        Some(Some(stream)) => Ok(stream),
        Some(None) if cancel.load(Ordering::Relaxed) => Err(ytdl::CANCELLED.into()),
        _ => {
            // The request now belongs to the foreground; do not also solve it
            // on the background queue.
            prefetch.drop_queued(url);
            ytdl::stream_url_cancelled(url, cfg, Some(cancel))
        }
    };
    if let Ok(stream) = &res {
        prefetch.remember(url, stream);
    }
    res
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

/// What a replacement sink starts with after the output device changed.
///
/// At the same rate the unplayed ring is handed over untouched, so a switch is
/// seamless. At a different rate the ring would need resampling; start the new
/// sink at the scaled position and let the decoder feed it from there.
fn handover(old_rate: u32, new_rate: u32, frames: u64, buffered: Vec<f32>) -> (Vec<f32>, u64) {
    if old_rate == new_rate {
        (buffered, frames)
    } else {
        let scaled = (frames as f64 * f64::from(new_rate) / f64::from(old_rate)) as u64;
        (Vec::new(), scaled)
    }
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
            device_check: Instant::now(),
            last_rebuild: Instant::now(),
            prefetch,
            prefetched: None,
            load: None,
            load_seq: 0,
            resolve_ms: ResolveTimes::default(),
            tail: None,
            tail_cut: None,
            tail_done: None,
            seek_gen: 0,
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

    /// Cancel the foreground resolve in flight, if any, killing its yt-dlp.
    fn supersede(&mut self) {
        if let Some(load) = self.load.take() {
            load.token.store(true, Ordering::Relaxed);
            drop(load.rx);
        }
    }

    /// Start resolving `url` off the audio thread. The caller has already put
    /// the UI into `Loading` with the track's metadata.
    fn begin_load(&mut self, url: String) {
        self.supersede();
        self.load_seq = self.load_seq.wrapping_add(1);
        let seq = self.load_seq;
        let token = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let cfg = self.cfg.clone();
        let prefetch = Arc::clone(&self.prefetch);
        let cancel = Arc::clone(&token);
        let target = url.clone();
        let spawned = thread::Builder::new()
            .name("ytui-load".into())
            .stack_size(512 * 1024)
            .spawn(move || {
                let _ = tx.send(load_stream(&target, &cfg, &prefetch, &cancel));
            });
        if spawned.is_err() {
            // Out of threads: do it here rather than not at all. Rare enough
            // that losing the responsiveness is the right trade.
            let res = load_stream(&url, &self.cfg.clone(), &self.prefetch, &token);
            self.complete(res);
            return;
        }
        self.load = Some(Load {
            seq,
            started: Instant::now(),
            token,
            rx,
        });
    }

    /// Applies a finished foreground resolve. Called from `tick`, so the engine
    /// loop keeps running while the answer is on its way.
    fn poll_load(&mut self) {
        let (seq, started, res) = {
            let Some(load) = &self.load else { return };
            match load.rx.try_recv() {
                Ok(res) => (load.seq, load.started, res),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    (load.seq, load.started, Err("the resolver stopped".into()))
                }
            }
        };
        if seq != self.load_seq {
            return;
        }
        if res.is_ok() {
            // What this machine really pays for a resolve, remote or local:
            // the prefetch lead follows it instead of a constant.
            self.resolve_ms.record(started.elapsed());
        }
        self.load = None;
        self.complete(res);
    }

    fn complete(&mut self, res: Result<ytdl::Stream, String>) {
        if matches!(&res, Err(e) if e == ytdl::CANCELLED) {
            // A newer selection took over; its own load reports anything real.
            return;
        }
        self.surface_note();
        let mut cached = false;
        let opened = res.and_then(|stream| {
            cached = stream.cached;
            // Entries created from a bare URL start out with no title or
            // duration; yt-dlp just told us both, so adopt them.
            self.adopt_metadata(&stream);
            Loaded::open(&stream.url, self.cfg.cache_kib as usize * 1024)
        });
        match opened {
            Ok(mut l) => {
                self.fails = 0;
                // Play at the track's rate when the device takes it; this
                // avoids resampling 44.1 kHz AAC up to 48 kHz and back.
                self.retune_sink(l.rate());
                l.set_target_rate(self.sink_rate());
                // A rate change replaces the sink, which starts out released.
                self.hold_at(0.0);

                // A pause pressed while the track was resolving must survive
                // the handover, so take the state from the sink, not Loading.
                let paused = self.sink.as_ref().map(|s| s.is_paused()).unwrap_or(false);
                let mut st = self.lock();
                st.status = if paused {
                    Status::Paused
                } else {
                    Status::Playing
                };
                if l.duration > 0.0 {
                    st.dur = l.duration;
                }
                if let Some(activity) = &mut st.activity {
                    activity.finish(cached);
                }
                drop(st);
                self.loaded = Some(l);
            }
            Err(e) => self.failed(e),
        }
    }

    /// Fills in a placeholder entry from what the resolve returned.
    fn adopt_metadata(&mut self, stream: &ytdl::Stream) {
        let mut changed = false;
        let mut st = self.lock();
        if st.title.is_empty() && !stream.title.is_empty() {
            st.title = stream.title.clone();
            changed = true;
        }
        if st.channel.is_empty() && !stream.channel.is_empty() {
            st.channel = stream.channel.clone();
            changed = true;
        }
        if st.dur <= 0.0 && stream.duration > 0.0 {
            st.dur = stream.duration;
            changed = true;
        }
        if changed {
            let index = st.index;
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
    }

    /// Marks the seek in the footer as done once the sound is back.
    fn finish_seek(&mut self) {
        let mut st = self.lock();
        if let Some(activity) = &mut st.activity {
            if activity.kind == ActivityKind::Seek {
                activity.finish(false);
            }
        }
    }

    fn failed(&mut self, e: String) {
        // One dead upload should not end the evening. Walk past tracks YouTube
        // refuses to serve, but stop short of hammering through a whole list.
        let (index, len) = {
            let st = self.lock();
            (st.index, st.queue.len())
        };
        if len > 1 && self.fails + 1 < MAX_SKIPS && track_is_gone(&e) {
            self.fails += 1;
            let mut st = self.lock();
            st.notice = Some(format!("{e}, skipped {}", st.title));
            st.notice_err = true;
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
        // Failures are the red toast's job; the footer goes quiet instead of
        // inventing a second vocabulary for them.
        st.activity = None;
    }

    /// Moves a remote-resolver failure into the player state, where the UI
    /// shows it as a single toast. The notice is consumed, so a server that
    /// stays down does not produce one per track.
    /// How early the next track is requested. Twice the measured cold resolve
    /// covers the handover and a retry; the floor covers the first track of a
    /// session, when there is nothing measured yet.
    fn resolve_lead(&self) -> f64 {
        resolve_lead(self.resolve_ms.median())
    }

    /// Scans the tail of the current track for a long run of silence, so the
    /// track can end there instead of playing it out. Runs once per position,
    /// on its own thread: the decoder must keep running.
    fn scan_tail(&mut self) {
        if self.tail.is_some() || self.tail_cut.is_some() {
            return;
        }
        let Some(loaded) = &self.loaded else { return };
        let dur = loaded.duration;
        if dur <= tail::SPAN_SECS {
            return;
        }
        let Some(sink) = &self.sink else { return };
        if sink.is_paused() {
            return;
        }
        let pos = sink.played_frames() as f64 / f64::from(sink.rate());
        if dur - pos > tail::SPAN_SECS {
            return;
        }
        let key = (self.lock().url.clone(), self.seek_gen);
        if self.tail_done.as_ref() == Some(&key) {
            return;
        }
        self.tail_done = Some(key.clone());
        let token = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let chunk = self.cfg.cache_kib as usize * 1024;
        let cancel = Arc::clone(&token);
        let url = key.0;
        let spawned = thread::Builder::new()
            .name("ytui-tail".into())
            .spawn(move || {
                let _ = tx.send(tail::scan(&url, dur, chunk, &cancel));
            });
        if spawned.is_ok() {
            self.tail = Some(TailScan { token, rx });
        }
    }

    fn poll_tail(&mut self) {
        let Some(scan) = &self.tail else { return };
        match scan.rx.try_recv() {
            Ok(cut) => {
                self.tail = None;
                self.tail_cut = cut;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                // The scan died; do not stand in the way of a later one.
                self.tail = None;
                self.tail_done = None;
            }
        }
    }

    /// Forgets the tail verdict: a new track or a seek lands somewhere else.
    fn reset_tail(&mut self) {
        if let Some(scan) = self.tail.take() {
            scan.token.store(true, Ordering::Relaxed);
        }
        self.tail_cut = None;
        self.tail_done = None;
        self.seek_gen = self.seek_gen.wrapping_add(1);
    }

    fn surface_note(&self) {
        if let Some(msg) = crate::remote::take_note() {
            self.note(msg);
        }
    }

    /// Shows `msg` once as an informational toast in the player.
    fn note(&self, msg: String) {
        let mut st = self.lock();
        st.notice = Some(msg);
        st.notice_err = false;
        st.notice_seq = st.notice_seq.wrapping_add(1);
    }

    /// Ask for the next track's URL once the current one is about to end.
    ///
    /// The lead is what this machine actually needs: a cold resolver takes
    /// about twenty seconds, so a fixed ten-second window would hand the next
    /// track to a resolver that has not finished yet. Twice the measured
    /// median plus a margin covers the handover, and the floor keeps a slow
    /// first resolve from being asked for too late even before there are
    /// samples. Only one track is solved ahead: a link that is not used soon
    /// would expire and have to be resolved again anyway.
    fn prefetch_next_near_end(&mut self) {
        // Escape hatch for slow machines (or for A/B testing a stutter):
        // `YTUI_PREFETCH=0 ytui` resolves every track on demand only.
        if std::env::var("YTUI_PREFETCH").ok().as_deref() == Some("0") {
            return;
        }
        if self.loaded.is_none() {
            return;
        }
        let Some(sink) = &self.sink else { return };
        // While paused the end may be hours away, and a link solved now could
        // expire before it is used.
        if sink.is_paused() {
            return;
        }
        let lead = self.resolve_lead();
        let pos = sink.played_frames() as f64 / f64::from(sink.rate());
        let (index, len, repeat, shuffle, state_dur) = {
            let st = self.lock();
            (st.index, st.queue.len(), st.repeat, st.shuffle, st.dur)
        };
        let dur = self
            .loaded
            .as_ref()
            .map(|l| l.duration)
            .filter(|d| *d > 0.0)
            .unwrap_or(state_dur);
        if dur <= 0.0 || pos >= dur || dur - pos > lead {
            return;
        }
        // Repeat-one rewinds the same stream; shuffle picks its next track
        // only when the current one ends.
        if repeat == Repeat::One || shuffle {
            return;
        }
        let next = next_index(index, len, repeat);
        let url = {
            let st = self.lock();
            next.and_then(|i| st.queue.get(i)).map(|t| t.url.clone())
        };
        let Some(url) = url else { return };
        if self.prefetched.as_deref() == Some(url.as_str()) {
            return;
        }
        self.prefetched = Some(url.clone());
        self.prefetch.request(vec![url]);
    }

    /// Reopens the output on the current default device, keeping the track
    /// where it was.
    ///
    /// The samples that were decoded but not played yet are handed to the new
    /// sink, so a device change does not cost the buffered seconds. If the
    /// track's rate is refused there, the device's own default rate is used and
    /// the decoder starts resampling to it.
    fn reopen_sink(&mut self) -> Result<(), String> {
        self.last_rebuild = Instant::now();
        let Some(old) = self.sink.as_ref() else {
            return Err("no output device is open".into());
        };
        let rate = old.rate();
        let (vol, muted) = {
            let st = self.lock();
            (st.volume, st.muted)
        };
        let new = match Sink::new(rate, self.cfg.buffer_ms) {
            Ok(s) => s,
            Err(first) => {
                let fallback = sink::Sink::default_rate().unwrap_or(OUT_RATE);
                if fallback == rate {
                    return Err(first);
                }
                Sink::new(fallback, self.cfg.buffer_ms)
                    .map_err(|e| format!("{first}; then {e}"))?
            }
        };
        let new_rate = new.rate();
        let paused = old.is_paused();
        let (buffered, frames) = handover(
            rate,
            new_rate,
            old.played_frames(),
            old.take_buffered(),
        );
        new.push(&buffered);
        new.set_played_frames(frames);
        new.set_volume(vol);
        new.set_muted(muted);
        new.set_paused(paused);
        if self.holding.is_some() {
            new.set_hold(true);
        }
        self.sink = Some(new);
        if let Some(loaded) = &mut self.loaded {
            loaded.set_target_rate(new_rate);
        }
        Ok(())
    }

    /// Moves playback to the new default output device when the system default
    /// changes while a stream is open.
    ///
    /// WASAPI binds a stream to the endpoint it was created on, so connecting
    /// AirPods or picking another output in the tray otherwise leaves the music
    /// on the old speakers. PulseAudio and ALSA's `default` PCM already reroute
    /// an open stream, so [`sink::default_device_name`] only answers on Windows.
    fn follow_default_output(&mut self) {
        if self.device_check.elapsed() < DEVICE_POLL {
            return;
        }
        self.device_check = Instant::now();
        let Some(name) = sink::default_device_name() else {
            return;
        };
        if self.sink.as_ref().is_some_and(|s| s.device_name() == name) {
            return;
        }
        if self.reopen_sink().is_ok() {
            let opened = self
                .sink
                .as_ref()
                .map(|s| s.device_name().to_string())
                .unwrap_or(name);
            self.note(format!("audio: {opened}"));
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
            Cmd::SetConfig(cfg) => {
                self.cfg = cfg.clone();
                self.prefetch.set_config(cfg);
            }
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
                let mut st = self.lock();
                st.queue.push(track);
                st.version = st.version.wrapping_add(1);
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
                } else if let Some(pause) = self.sink.as_ref().map(|s| !s.is_paused()) {
                    self.set_paused(pause);
                }
            }
            Cmd::Resume => {
                let (status, index) = {
                    let st = self.lock();
                    (st.status, st.index)
                };
                match status {
                    Status::Paused => self.set_paused(false),
                    // After a stop or a failed track, play means "start this
                    // one again"; `start` is a no-op on an empty queue.
                    Status::Stopped | Status::Error => {
                        self.fails = 0;
                        self.start(index);
                    }
                    _ => {}
                }
            }
            Cmd::Pause => {
                if self.lock().status == Status::Playing {
                    self.set_paused(true);
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
                self.supersede();
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
                st.activity = None;
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

    /// Pause or resume the output without touching the queue. The status is
    /// only changed for a track that is actually playing or paused, so a
    /// media key during a resolve cannot fake a playback state.
    fn set_paused(&mut self, pause: bool) {
        let Some(sink) = &self.sink else {
            return;
        };
        sink.set_paused(pause);
        if pause {
            // The end may now be hours away, so a link solved before the
            // pause could expire. Let the next tick request it again after
            // resuming.
            self.prefetched = None;
        }
        let mut st = self.lock();
        if matches!(st.status, Status::Playing | Status::Paused) {
            st.status = if pause {
                Status::Paused
            } else {
                Status::Playing
            };
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
        self.reset_tail();
        self.lock().activity = Some(Activity::new(ActivityKind::Seek, mmss(actual)));
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
        self.prefetched = None;
        self.reset_tail();
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
            // A pasted link has no title yet; the footer still needs a name.
            st.activity = Some(Activity::new(
                ActivityKind::Resolve,
                if track.title.trim().is_empty() {
                    "track".to_string()
                } else {
                    track.title.clone()
                },
            ));
        }

        self.begin_load(track.url);
    }

    fn fail(&mut self, err: String) {
        self.supersede();
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
        let sink_err = self.sink.as_ref().and_then(|s| s.take_error());
        if let Some(e) = sink_err {
            // A stream dies when its endpoint goes away (headphones unplugged,
            // Bluetooth off). Try the current default first and only report the
            // failure when no device can be opened.
            if self.last_rebuild.elapsed() >= DEVICE_POLL && self.reopen_sink().is_ok() {
                let opened = self
                    .sink
                    .as_ref()
                    .map(|s| s.device_name().to_string())
                    .unwrap_or_default();
                self.note(format!("audio: {opened}"));
            } else {
                self.fail(e);
                return;
            }
        }
        self.follow_default_output();
        // A finished foreground resolve is opened here, where the decoder and
        // sink live.
        self.poll_load();
        self.poll_tail();

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
                    self.finish_seek();
                }
            }
        }

        // Near the end of the track, solve the next URL so the handover is
        // short. This runs every tick; the function itself is a no-op until
        // the last seconds and only requests each next track once.
        self.prefetch_next_near_end();
        self.scan_tail();

        // A track whose ending is silence stops at the start of that run. The
        // cut is always inside a run below -45 dBFS that lasts at least seven
        // seconds, so there is nothing audible at the boundary to click.
        if let Some(cut) = self.tail_cut {
            let pos = self
                .sink
                .as_ref()
                .map(|s| s.played_frames() as f64 / f64::from(s.rate()))
                .unwrap_or(0.0);
            if pos >= cut {
                self.tail_cut = None;
                self.advance();
                return;
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
                    self.reset_tail();
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

/// How early the next track is requested, from the median cold resolve time.
///
/// A fixed ten-second window assumed a local yt-dlp; a resolver on the LAN or
/// over Tailscale can take twenty seconds cold, and asking late is heard as a
/// gap between songs. Twice the measured time plus a margin covers the
/// resolve, the probe and the handover; the floor keeps the first track of a
/// session from being asked for too late.
fn resolve_lead(median: Option<Duration>) -> f64 {
    let measured = median.map(|d| d.as_secs_f64()).unwrap_or(0.0);
    (measured * 2.0 + RESOLVE_LEAD_MARGIN_SECS).max(RESOLVE_LEAD_MIN_SECS)
}

/// Index of the track that plays after `index`, or `None` when playback stops
/// at the end of the queue. Used to pick the one URL worth resolving ahead.
fn next_index(index: usize, len: usize, repeat: Repeat) -> Option<usize> {
    if index + 1 < len {
        Some(index + 1)
    } else if repeat == Repeat::All && len > 0 {
        Some(0)
    } else {
        None
    }
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
    fn a_same_rate_device_change_keeps_the_unplayed_ring() {
        let (ring, frames) = handover(44_100, 44_100, 1234, vec![0.1, 0.2, 0.3]);
        assert_eq!(ring, [0.1, 0.2, 0.3]);
        assert_eq!(frames, 1234);
    }

    #[test]
    fn a_rate_change_moves_the_position_and_drops_the_old_ring() {
        let (ring, frames) = handover(44_100, 48_000, 44_100, vec![0.1, 0.2]);
        assert!(ring.is_empty());
        assert_eq!(frames, 48_000);
    }

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
            cached: false,
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
    fn only_the_following_track_is_solved_ahead() {
        assert_eq!(next_index(0, 3, Repeat::Off), Some(1));
        assert_eq!(next_index(1, 3, Repeat::Off), Some(2));
        // The last track: stop, or wrap only when the queue repeats.
        assert_eq!(next_index(2, 3, Repeat::Off), None);
        assert_eq!(next_index(2, 3, Repeat::All), Some(0));
        assert_eq!(next_index(0, 0, Repeat::All), None);
    }

    #[test]
    fn the_footer_shows_a_resolve_and_how_long_it_took() {
        let mut a = Activity::new(ActivityKind::Resolve, "Nightcall".into());
        assert_eq!(a.line().as_deref(), Some("⏳ Nightcall"));

        a.started = Instant::now() - Duration::from_secs(2);
        a.finish(false);
        assert_eq!(a.line().as_deref(), Some("✅ Nightcall · 2.0s"));
    }

    #[test]
    fn the_cache_marker_only_shows_on_a_hit() {
        let mut a = Activity::new(ActivityKind::Resolve, "Nightcall".into());
        a.started = Instant::now() - Duration::from_secs(13);
        a.finish(true);
        assert_eq!(a.line().as_deref(), Some("✅ Nightcall · 13s · 📦"));
    }

    #[test]
    fn a_seek_shows_its_target_and_then_its_time() {
        let mut a = Activity::new(ActivityKind::Seek, mmss(84.6));
        assert_eq!(a.line().as_deref(), Some("⏩ 1:24"));
        a.started = Instant::now() - Duration::from_millis(320);
        a.finish(false);
        assert_eq!(a.line().as_deref(), Some("✅ 1:24 · 0.3s"));
    }

    #[test]
    fn a_finished_line_ages_out() {
        let mut a = Activity::new(ActivityKind::Seek, "1:24".into());
        a.finish(false);
        assert!(a.line().is_some());
        if let Some(done) = &mut a.done {
            done.at = Instant::now() - Duration::from_secs(4);
        }
        assert!(a.line().is_none());
    }

    #[test]
    fn the_resolve_lead_follows_what_this_machine_measures() {
        // Nothing measured yet: the cold-resolver floor.
        assert_eq!(resolve_lead(None), 20.0);
        // A fast local yt-dlp does not bring the lead below the floor.
        assert_eq!(resolve_lead(Some(Duration::from_secs(3))), 20.0);
        // Twenty seconds cold: ask forty-five seconds ahead.
        assert_eq!(resolve_lead(Some(Duration::from_secs(20))), 45.0);
        assert_eq!(resolve_lead(Some(Duration::from_secs(60))), 125.0);
    }

    #[test]
    fn a_cancelled_load_never_starts_ytdlp() {
        let p = Prefetch {
            inner: Mutex::new(Job::default()),
            cv: Condvar::new(),
            cfg: Mutex::new(Config::default()),
        };
        let cancel = AtomicBool::new(true);
        assert_eq!(
            load_stream("x", &Config::default(), &p, &cancel).unwrap_err(),
            ytdl::CANCELLED
        );
    }

    #[test]
    fn a_cached_url_loads_without_the_network() {
        let p = Prefetch {
            inner: Mutex::new(Job::default()),
            cv: Condvar::new(),
            cfg: Mutex::new(Config::default()),
        };
        let link = link(now() + 3600);
        remember(&mut p.lock(), "x", stream(&link));
        let cancel = AtomicBool::new(false);
        let got = load_stream("x", &Config::default(), &p, &cancel).unwrap();
        assert_eq!(got.url, link);
    }

    #[test]
    fn a_jump_replaces_the_prefetch_queue() {
        let p = Prefetch {
            inner: Mutex::new(Job::default()),
            cv: Condvar::new(),
            cfg: Mutex::new(Config::default()),
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
