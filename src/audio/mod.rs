//! Playback engine: one worker thread owns the decoder, the queue and the
//! audio sink. The UI talks to it through `Cmd` messages and reads `State`
//! through a mutex, so drawing never blocks on I/O.
//!
//! Why a custom engine instead of mpv: no second process, no IPC socket, no
//! video pipeline. Idle cost is the ratatui buffer plus a ~1 MB audio ring.

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

pub const OUT_RATE: u32 = 48_000;

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
    pub pos: f64,
    pub dur: f64,
    pub volume: i64,
    pub muted: bool,
    pub queue: Vec<Track>,
    pub index: usize,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub err: Option<String>,
    /// Bumped whenever the queue contents change, so the UI can cache rows.
    pub version: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            status: Status::Stopped,
            title: String::new(),
            channel: String::new(),
            pos: 0.0,
            dur: 0.0,
            volume: 100,
            muted: false,
            queue: Vec::new(),
            index: 0,
            shuffle: false,
            repeat: Repeat::Off,
            err: None,
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
    tmp: Vec<f32>,
    target_frames: usize,
    rng: u64,
}

impl Engine {
    fn new(cfg: Config, state: Arc<Mutex<State>>) -> Self {
        let mut engine = Self {
            target_frames: (OUT_RATE as usize * cfg.buffer_ms as usize / 1000).max(4096),
            cfg,
            state,
            sink: None,
            loaded: None,
            eof: false,
            tmp: Vec::with_capacity(16384),
            rng: seed(),
        };
        match Sink::new(engine.cfg.buffer_ms) {
            Ok(s) => {
                {
                    // Start quiet: a TUI player has no reason to blast the
                    // first track at full scale.
                    let mut st = engine.lock();
                    st.volume = engine.cfg.volume.clamp(0, 130);
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
                self.start(index);
            }
            Cmd::Append(track) => {
                let mut st = self.lock();
                st.queue.push(track);
                st.version = st.version.wrapping_add(1);
            }
            Cmd::Toggle => {
                if let Some(s) = &self.sink {
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
                    / f64::from(OUT_RATE);
                self.seek_to(pos + delta);
            }
            Cmd::SeekAbs(secs) => self.seek_to(secs),
            Cmd::Volume(v) => {
                let v = v.clamp(0, 130);
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
            Cmd::Next => self.skip(1),
            Cmd::Prev => self.skip(-1),
            Cmd::PlayIndex(i) => {
                let len = self.lock().queue.len();
                if i < len {
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
            s.set_played_frames((actual * f64::from(OUT_RATE)) as u64);
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
            st.pos = 0.0;
            st.dur = track.duration as f64;
            st.err = None;
        }

        let loaded = ytdl::stream_url(&track.url, &self.cfg).and_then(|stream| {
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
            Ok(l) => {
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
            }
            Err(e) => {
                let mut st = self.lock();
                st.status = Status::Error;
                st.err = Some(e);
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
                if sink.buffered_frames() >= self.target_frames {
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
            let pos = s.played_frames() as f64 / f64::from(OUT_RATE);
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

/// Opens the default output device for a moment; used by `ytui doctor`.
pub fn check_device() -> Result<String, String> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no default output device".to_string())?;
    let name = device
        .name()
        .unwrap_or_else(|_| "unknown device".to_string());
    // Merely existing is not enough: the device has to actually start.
    let _sink = sink::Sink::new(200)?;
    Ok(format!("{name} @ 48 kHz stereo"))
}

fn seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    nanos ^ (std::process::id() as u64) << 32
}
