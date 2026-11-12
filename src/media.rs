//! OS media controls.
//!
//! ytui decodes and plays audio itself, so there is no AVPlayer or mpv session
//! to attach the media keys to. Each desktop routes the keys to the current
//! media session instead, and ytui publishes playback there: SMTC on Windows,
//! MPRIS on Linux and Now Playing on macOS. WSL has no session of its own, so
//! the Linux build drives the native Windows `ytui.exe` over a pipe.
//!
//! On macOS the MediaPlayer work stays on the UI thread, where the run loop
//! already lives. The other targets own a thread (or a child process, under
//! WSL) and hand the UI thread nothing but channels.

use crate::audio::{Cmd, State, Status};
use std::sync::mpsc::Sender;

#[cfg(not(target_os = "macos"))]
mod engine;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod native;
#[cfg(target_os = "windows")]
mod serve;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(any(target_os = "linux", target_os = "windows"))]
mod wire;
#[cfg(target_os = "linux")]
mod wsl;
#[cfg(target_os = "linux")]
mod x11;

#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(target_os = "windows")]
use windows as platform;

/// The D-Bus / WinRT name shown to the desktop. Kept short so it reads well in
/// GNOME's media menu and Windows' volume flyout.
pub const APP_NAME: &str = "ytui";

/// What the OS should show as the current playback state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    Stopped,
    Playing,
    Paused,
}

/// Everything the OS needs for its media widget. Published once a second (and
/// immediately when the track, status or volume changes).
#[derive(Debug, Clone)]
pub struct NowPlaying {
    pub playback: Playback,
    pub title: String,
    pub artist: String,
    pub duration: f64,
    pub position: f64,
    /// 0-100, the same scale as `Config::volume`.
    pub volume: i64,
    pub muted: bool,
    pub shuffle: bool,
}

impl NowPlaying {
    fn of(st: &State) -> Self {
        let playback = match st.status {
            Status::Playing | Status::Loading => Playback::Playing,
            Status::Paused => Playback::Paused,
            Status::Stopped | Status::Error => Playback::Stopped,
        };
        Self {
            playback,
            title: st.title.clone(),
            artist: st.channel.clone(),
            duration: st.dur,
            position: st.pos,
            volume: st.volume,
            muted: st.muted,
            shuffle: st.shuffle,
        }
    }
}

impl PartialEq for NowPlaying {
    fn eq(&self, other: &Self) -> bool {
        // The progress bar only needs one update per second; comparing the
        // exact position would republish on every tick.
        self.playback == other.playback
            && self.title == other.title
            && self.artist == other.artist
            && self.duration == other.duration
            && self.volume == other.volume
            && self.muted == other.muted
            && self.shuffle == other.shuffle
            && self.position as u64 == other.position as u64
    }
}

/// A transport request from the OS.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    Play,
    Pause,
    Toggle,
    Next,
    Prev,
    Stop,
    /// Toggle mute (X11 fallback; desktops normally keep the mute key).
    Mute,
    /// Relative seek in seconds (negative rewinds).
    Seek(f64),
    /// Absolute position in seconds.
    SeekAbs(f64),
    /// Absolute volume, 0.0-1.0 (MPRIS).
    Volume(f64),
    Shuffle(bool),
    Quit,
}

/// The part of the player state macOS needs. Comparing it keeps publishing
/// down to the moments something actually changes.
#[cfg(target_os = "macos")]
#[derive(Clone)]
pub struct Now {
    pub status: Status,
    pub title: String,
    pub channel: String,
    pub url: String,
    pub pos: f64,
    pub dur: f64,
}

#[cfg(target_os = "macos")]
impl Now {
    fn of(st: &State) -> Self {
        Self {
            status: st.status,
            title: st.title.clone(),
            channel: st.channel.clone(),
            url: st.url.clone(),
            pos: st.pos,
            dur: st.dur,
        }
    }
}

#[cfg(target_os = "macos")]
impl PartialEq for Now {
    fn eq(&self, other: &Self) -> bool {
        // The progress bar only needs one update per second; comparing the
        // exact position would republish on every tick.
        self.status == other.status
            && self.title == other.title
            && self.channel == other.channel
            && self.url == other.url
            && self.dur == other.dur
            && self.pos as u64 == other.pos as u64
    }
}

pub struct Media {
    #[cfg(target_os = "macos")]
    inner: macos::Session,
    #[cfg(target_os = "macos")]
    last: Option<Now>,
    /// `None` when the platform has no usable session or the config turned
    /// the integration off; the player works without it.
    #[cfg(not(target_os = "macos"))]
    inner: Option<engine::Controls>,
    #[cfg(not(target_os = "macos"))]
    last: Option<NowPlaying>,
    #[cfg(not(target_os = "macos"))]
    cmd_tx: Sender<Cmd>,
    /// Set by a desktop `Quit` command; the UI loop picks it up and stops.
    #[cfg(not(target_os = "macos"))]
    quit: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Media {
    pub fn new(cmd_tx: Sender<Cmd>, enabled: bool) -> Self {
        #[cfg(target_os = "macos")]
        {
            let _ = enabled;
            Self {
                inner: macos::Session::new(cmd_tx),
                last: None,
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            Self {
                inner: enabled.then(engine::Controls::start).flatten(),
                last: None,
                cmd_tx,
                quit: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }
    }

    /// Publish the current player state if it changed, and apply any buttons
    /// pressed in the OS widget. Publishing only happens on a real change, so
    /// calling this every frame is cheap.
    pub fn sync(&mut self, st: &State) {
        #[cfg(target_os = "macos")]
        {
            let now = Now::of(st);
            if self.last.as_ref() != Some(&now) {
                self.last = Some(now.clone());
                self.inner.show(now);
            }
            // The run loop is pumped on every call so media keys keep working
            // even while nothing changes.
            self.inner.pump();
        }
        #[cfg(not(target_os = "macos"))]
        {
            if self.inner.is_none() {
                return;
            }
            let now = NowPlaying::of(st);
            if self.last.as_ref() != Some(&now) {
                self.last = Some(now.clone());
                if let Some(controls) = &mut self.inner {
                    controls.update(now);
                }
            }
            while let Some(event) = self.inner.as_ref().and_then(engine::Controls::try_event) {
                self.apply(event, st);
            }
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn apply(&mut self, event: Event, st: &State) {
        let cmd = match event {
            Event::Play => Cmd::Resume,
            Event::Pause => Cmd::Pause,
            Event::Toggle => Cmd::Toggle,
            Event::Next => Cmd::Next,
            Event::Prev => Cmd::Prev,
            Event::Stop => Cmd::Stop,
            Event::Mute => Cmd::Mute(!st.muted),
            Event::Seek(seconds) => Cmd::Seek(seconds),
            Event::SeekAbs(seconds) => Cmd::SeekAbs(seconds),
            Event::Volume(volume) => Cmd::Volume((volume.clamp(0.0, 1.0) * 100.0).round() as i64),
            Event::Shuffle(on) => Cmd::Shuffle(on),
            Event::Quit => {
                self.quit
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                return;
            }
        };
        let _ = self.cmd_tx.send(cmd);
    }

    /// True when the desktop asked ytui to quit since the last call.
    pub fn quit(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            false
        }
        #[cfg(not(target_os = "macos"))]
        {
            self.quit.swap(false, std::sync::atomic::Ordering::Relaxed)
        }
    }

    /// Remove the Now Playing entry (macOS) or stop the session thread.
    pub fn shutdown(&mut self) {
        #[cfg(target_os = "macos")]
        self.inner.shutdown();
        #[cfg(not(target_os = "macos"))]
        {
            self.inner = None;
        }
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// A line for `ytui doctor`: the session that registers, or why none does.
pub fn doctor_line() -> String {
    #[cfg(target_os = "macos")]
    {
        "✔ media keys: macOS Now Playing (media keys, Control Center, Touch Bar)".to_string()
    }
    #[cfg(not(target_os = "macos"))]
    {
        match engine::Controls::start() {
            Some(controls) => format!("✔ media keys: {}", controls.mode()),
            None => format!(
                "· media keys: {} (could not register right now)",
                engine::status()
            ),
        }
    }
}

/// `ytui --media-bridge`: the hidden Windows side of the WSL bridge. Reads
/// state updates as JSON lines on stdin and writes button presses as JSON
/// lines on stdout. Never draws anything.
pub fn serve_bridge() -> i32 {
    #[cfg(target_os = "windows")]
    {
        serve::run()
    }
    #[cfg(not(target_os = "windows"))]
    {
        eprintln!("ytui: --media-bridge is an internal Windows helper for WSL");
        2
    }
}
