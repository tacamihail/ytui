//! Platform media sessions, behind `crate::media`.
//!
//! A terminal program never sees the media keys itself. Each desktop routes
//! them to the current media session instead: SMTC on Windows and MPRIS on
//! Linux. The platform backends publish ytui's state there and turn the
//! buttons back into player commands. WSL has no desktop media session of its
//! own, so there the Linux build drives the native Windows `ytui.exe` over a
//! pipe.

use super::platform;
use super::{Event, NowPlaying};
use std::sync::mpsc::{self, Receiver};

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

/// One line for `ytui doctor` when no session could be started.
pub fn status() -> String {
    #[cfg(target_os = "windows")]
    {
        "Windows SMTC (media keys, media flyout, lock screen)".to_string()
    }
    #[cfg(target_os = "linux")]
    {
        super::linux::status()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        "no OS media session on this platform".to_string()
    }
}
