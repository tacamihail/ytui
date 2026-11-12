//! OS media sessions: keyboard media keys, headphone buttons, lock-screen /
//! Control Center metadata and transport controls.
//!
//! A terminal program never sees the media keys itself. Each desktop routes
//! them to the current media session instead: SMTC on Windows, MPRIS on Linux
//! and Now Playing on macOS. The platform backends publish ytui's state there
//! and turn the buttons back into player commands. WSL has no desktop media
//! session of its own, so there the Linux build drives the native Windows
//! `ytui.exe` over a pipe.

use std::sync::mpsc::{self, Receiver};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "linux", target_os = "macos"))]
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
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "windows")]
use windows as platform;

/// What the OS should show as the current playback state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Playback {
    Stopped,
    Playing,
    Paused,
}

/// Everything the OS needs for its media widget. Sent once a second (and
/// immediately when the track, status or volume changes).
#[derive(Debug, Clone, PartialEq)]
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

/// Handle to the platform media session. `None` when the platform has no
/// usable session; ytui keeps working without it.
pub struct Controls {
    inner: platform::Backend,
    events: Receiver<Event>,
    mode: String,
}

impl Controls {
    pub fn start() -> Option<Self> {
        let (tx, events) = mpsc::channel();
        platform::Backend::start(tx).map(|inner| {
            let mode = inner.mode();
            Self {
                inner,
                events,
                mode,
            }
        })
    }

    /// Human-readable name of the backend that actually registered. Used by
    /// `ytui doctor` so WSL users can see whether the bridge or the X11 grab
    /// won.
    pub fn mode(&self) -> &str {
        &self.mode
    }

    pub fn update(&mut self, now: NowPlaying) {
        self.inner.update(now);
    }

    pub fn try_event(&self) -> Option<Event> {
        self.events.try_recv().ok()
    }
}

/// One line for `ytui doctor`.
pub fn status() -> String {
    #[cfg(target_os = "windows")]
    {
        "Windows SMTC (media keys, media flyout, lock screen)".to_string()
    }
    #[cfg(target_os = "macos")]
    {
        "macOS Now Playing (media keys, Control Center, Touch Bar)".to_string()
    }
    #[cfg(target_os = "linux")]
    {
        linux::status()
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

/// Give AppKit a slice of the main thread, once per TUI loop iteration.
/// macOS delivers Now Playing commands through the application run loop, and
/// the terminal event loop never runs one by itself. A no-op elsewhere.
pub fn pump() {
    #[cfg(target_os = "macos")]
    macos::pump();
}

/// The D-Bus / WinRT name shown to the desktop. Kept short so it reads well in
/// GNOME's media menu and Windows' volume flyout.
pub const APP_NAME: &str = "ytui";
