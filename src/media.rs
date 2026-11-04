//! OS media controls.
//!
//! ytui decodes and plays audio itself, so macOS has no AVPlayer or mpv
//! session to attach the media keys to. On macOS this publishes a Now Playing
//! item and answers the remote commands the system sends for F7/F8/F9, the
//! Control Center transport and headset buttons.
//!
//! The MediaPlayer work stays on the UI thread, where the run loop already
//! lives; other targets get a no-op `Media`.

use crate::audio::{Cmd, State, Status};
use std::sync::mpsc::Sender;

#[cfg(target_os = "macos")]
mod macos;

/// The part of the player state the system needs. Comparing it keeps
/// publishing down to the moments something actually changes.
#[derive(Clone)]
pub struct Now {
    pub status: Status,
    pub title: String,
    pub channel: String,
    pub url: String,
    pub pos: f64,
    pub dur: f64,
}

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
    #[cfg(not(target_os = "macos"))]
    _private: (),
    last: Option<Now>,
}

impl Media {
    pub fn new(cmd_tx: Sender<Cmd>) -> Self {
        #[cfg(target_os = "macos")]
        {
            Self {
                inner: macos::Session::new(cmd_tx),
                last: None,
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = cmd_tx;
            Self {
                _private: (),
                last: None,
            }
        }
    }

    /// Publish the current player state if it changed. The run loop is pumped
    /// on every call so media keys keep working even while nothing changes.
    pub fn sync(&mut self, st: &State) {
        let now = Now::of(st);
        if self.last.as_ref() != Some(&now) {
            self.last = Some(now.clone());
            #[cfg(target_os = "macos")]
            self.inner.show(now);
            #[cfg(not(target_os = "macos"))]
            let _ = now;
        }
        #[cfg(target_os = "macos")]
        self.inner.pump();
    }

    /// Remove the Now Playing entry.
    pub fn shutdown(&mut self) {
        #[cfg(target_os = "macos")]
        self.inner.shutdown();
    }
}

impl Drop for Media {
    fn drop(&mut self) {
        self.shutdown();
    }
}
