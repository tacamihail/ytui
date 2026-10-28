//! Playback engine. One worker thread owns the decoder, queue and sink; the UI
//! sends `Cmd` messages and reads `State` behind a mutex, so drawing never
//! waits on the network or the audio device.

mod decode;
mod http;
mod resample;
mod sink;

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

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
    /// Asks the resolver thread to work out the next track's audio URL while
    /// the current one plays. `yt-dlp` takes a few seconds per track, which is
    /// most of the pause between songs.
    prefetch_tx: Sender<String>,
    prefetched: Arc<Mutex<Option<(String, ytdl::Stream)>>>,
    tmp: Vec<f32>,
    rng: u64,
}

/// One background thread resolves URLs; `yt-dlp` is a subprocess, so this only
/// costs the resolved URL in memory (a few hundred bytes).
fn start_prefetcher(cfg: Config) -> (Sender<String>, Arc<Mutex<Option<(String, ytdl::Stream)>>>) {
    let (tx, rx) = mpsc::channel::<String>();
    let slot = Arc::new(Mutex::new(None));
    let out = Arc::clone(&slot);
    let _ = thread::Builder::new()
        .name("ytui-prefetch".into())
        .stack_size(512 * 1024)
        .spawn(move || {
            for url in rx {
                let Ok(stream) = ytdl::stream_url(&url, &cfg) else {
                    continue;
                };
                let mut g = out.lock().unwrap_or_else(|e| e.into_inner());
                *g = Some((url, stream));
            }
        });
    (tx, slot)
}

impl Engine {
    fn new(cfg: Config, state: Arc<Mutex<State>>) -> Self {
        let (prefetch_tx, prefetched) = start_prefetcher(cfg.clone());
        let mut engine = Self {
            cfg,
            state,
            sink: None,
            loaded: None,
            eof: false,
            fails: 0,
            prefetch_tx,
            prefetched,
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

    /// The next track's stream, if the background thread already worked it out.
    fn take_prefetched(&self, url: &str) -> Option<ytdl::Stream> {
        let mut g = self.prefetched.lock().unwrap_or_else(|e| e.into_inner());
        match g.as_ref() {
            Some((u, _)) if u == url => g.take().map(|(_, s)| s),
            _ => None,
        }
    }

    fn resolve(&self, url: &str) -> Result<ytdl::Stream, String> {
        match self.take_prefetched(url) {
            Some(s) => Ok(s),
            None => ytdl::stream_url(url, &self.cfg),
        }
    }

    /// Ask for the next track's URL once the current one is known to play.
    /// Resolving takes yt-dlp a few seconds, and that pause between songs is
    /// what people notice most.
    fn prefetch_after(&self, index: usize) {
        let next = {
            let st = self.lock();
            let len = st.queue.len();
            let i = match st.repeat {
                Repeat::One => Some(index),
                _ if st.shuffle => None,
                _ if index + 1 < len => Some(index + 1),
                Repeat::All if len > 0 => Some(0),
                _ => None,
            };
            i.and_then(|i| st.queue.get(i)).map(|t| t.url.clone())
        };
        let Some(url) = next else { return };
        let already = self
            .prefetched
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|(u, _)| u.as_str() == url.as_str());
        if !already {
            let _ = self.prefetch_tx.send(url);
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
                if let Some(s) = &self.sink {
                    s.clear();
                    s.set_played_frames(0);
                    s.set_paused(false);
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
        if let Some(s) = &self.sink {
            s.clear();
            s.set_played_frames((actual * f64::from(s.rate())) as u64);
        }
        self.lock().pos = actual;
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
            s.clear();
            s.set_played_frames(0);
            s.set_paused(false);
        }
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
        if let Some(s) = &self.sink {
            s.clear();
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

        // Pump the decoder while the ring buffer has room.
        if self.loaded.is_some() && !self.eof {
            loop {
                let Some(sink) = &self.sink else { break };
                if sink.buffered_frames() >= self.target_frames() {
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
                    if let Some(s) = &self.sink {
                        s.clear();
                        s.set_played_frames(0);
                    }
                    let mut st = self.lock();
                    st.pos = 0.0;
                    st.status = Status::Playing;
                    return;
                }
            }
        }

        self.eof = false;
        self.loaded = None;
        if let Some(s) = &self.sink {
            s.clear();
            s.set_played_frames(0);
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

/// Opens the default output device for a moment; used by `ytui doctor`.
pub fn check_device() -> Result<String, String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no default output device".to_string())?;
    let name = device
        .name()
        .unwrap_or_else(|e| format!("unknown device ({e})"));
    let rate = device
        .default_output_config()
        .map(|c| c.sample_rate().0)
        .unwrap_or(OUT_RATE);
    // Merely existing is not enough: the device has to actually start.
    let _sink = sink::Sink::new(rate, 200)?;
    Ok(format!("{name} @ {rate} Hz stereo"))
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
}
