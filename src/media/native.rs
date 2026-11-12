//! Linux (MPRIS) and macOS (Now Playing) backends, both through souvlaki.
//!
//! The desktop owns the media keys and asks the active media session to act.
//! Publishing playback state here is what makes the keys work: there is no
//! terminal escape sequence involved.

use super::{Event, NowPlaying, Playback};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use std::sync::mpsc::Sender;
use std::time::Duration;

/// Relative seek for a plain forward/rewind button, which does not say how far.
const SEEK_STEP: f64 = 10.0;

struct Meta {
    title: String,
    artist: String,
    duration: u64,
}

pub struct Backend {
    controls: MediaControls,
    meta: Option<Meta>,
    #[cfg(target_os = "linux")]
    volume: Option<i64>,
    #[cfg(target_os = "macos")]
    shuffle: Option<bool>,
}

impl Backend {
    pub fn start(events: Sender<Event>) -> Option<Self> {
        let mut controls = MediaControls::new(PlatformConfig {
            display_name: super::APP_NAME,
            dbus_name: super::APP_NAME,
            hwnd: None,
        })
        .ok()?;
        controls
            .attach(move |event| {
                if let Some(event) = map_event(event) {
                    let _ = events.send(event);
                }
            })
            .ok()?;
        Some(Self {
            controls,
            meta: None,
            #[cfg(target_os = "linux")]
            volume: None,
            #[cfg(target_os = "macos")]
            shuffle: None,
        })
    }

    pub fn mode(&self) -> String {
        if cfg!(target_os = "macos") {
            "macOS Now Playing".to_string()
        } else {
            "MPRIS".to_string()
        }
    }

    pub fn update(&mut self, now: NowPlaying) {
        let meta = Meta {
            title: now.title,
            artist: now.artist,
            duration: now.duration.max(0.0) as u64,
        };
        let fresh = match &self.meta {
            Some(old) => {
                old.title != meta.title
                    || old.artist != meta.artist
                    || old.duration != meta.duration
            }
            None => true,
        };
        if fresh {
            let duration = (meta.duration > 0).then(|| Duration::from_secs(meta.duration));
            let ok = self
                .controls
                .set_metadata(MediaMetadata {
                    title: Some(&meta.title),
                    artist: Some(&meta.artist),
                    album: None,
                    cover_url: None,
                    duration,
                })
                .is_ok();
            // Retry on the next tick if the bus went away, instead of
            // pretending the old title is current.
            if ok {
                self.meta = Some(meta);
            }
        }

        let progress = Some(MediaPosition(Duration::from_secs_f64(
            now.position.max(0.0),
        )));
        let playback = match now.playback {
            Playback::Stopped => MediaPlayback::Stopped,
            Playback::Playing => MediaPlayback::Playing { progress },
            Playback::Paused => MediaPlayback::Paused { progress },
        };
        let _ = self.controls.set_playback(playback);

        // MPRIS clients (playerctl, desktop sliders) set the volume through the
        // session, and the spec asks the player to confirm it.
        #[cfg(target_os = "linux")]
        {
            let volume = if now.muted {
                0
            } else {
                now.volume.clamp(0, 100)
            };
            if self.volume != Some(volume) {
                if self.controls.set_volume(volume as f64 / 100.0).is_ok() {
                    self.volume = Some(volume);
                }
            }
        }

        // macOS exposes shuffle as a remote command, unlike the other backends.
        #[cfg(target_os = "macos")]
        {
            if self.shuffle != Some(now.shuffle) {
                if self.controls.set_shuffle(Some(now.shuffle)).is_ok() {
                    self.shuffle = Some(now.shuffle);
                }
            }
        }
    }
}

fn map_event(event: MediaControlEvent) -> Option<Event> {
    match event {
        MediaControlEvent::Play => Some(Event::Play),
        MediaControlEvent::Pause => Some(Event::Pause),
        MediaControlEvent::Toggle => Some(Event::Toggle),
        MediaControlEvent::Next => Some(Event::Next),
        MediaControlEvent::Previous => Some(Event::Prev),
        MediaControlEvent::Stop => Some(Event::Stop),
        MediaControlEvent::Seek(direction) => Some(Event::Seek(seek_amount(direction, SEEK_STEP))),
        MediaControlEvent::SeekBy(direction, amount) => {
            Some(Event::Seek(seek_amount(direction, amount.as_secs_f64())))
        }
        MediaControlEvent::SetPosition(position) => Some(Event::SeekAbs(position.0.as_secs_f64())),
        MediaControlEvent::SetVolume(volume) => Some(Event::Volume(volume)),
        MediaControlEvent::SetShuffle(on) => Some(Event::Shuffle(on)),
        MediaControlEvent::Quit => Some(Event::Quit),
        // OpenUri and Raise have no meaning for a terminal player; unknown
        // variants may be added by newer souvlaki versions.
        _ => None,
    }
}

fn seek_amount(direction: SeekDirection, amount: f64) -> f64 {
    match direction {
        SeekDirection::Forward => amount,
        SeekDirection::Backward => -amount,
    }
}
