//! macOS Now Playing backend, behind `crate::media`.
//!
//! The MediaPlayer objects live on the UI thread. `MPRemoteCommandCenter`
//! delivers media-key and Control Center events through the main run loop, so
//! `pump` gives the run loop a moment on every frame; the ratatui loop is the
//! only thing on that thread, so this does not interfere with terminal input.
//!
//! Artwork is the exception: downloading a thumbnail takes long enough to
//! stutter the UI, so a short-lived thread fetches it and `pump` publishes it
//! once it arrives.

use crate::audio::{Cmd, Status};
use crate::media::Now;
use block2::RcBlock;
use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::AnyObject;
use objc2::{msg_send, AnyThread};
use objc2_app_kit::NSImage;
use objc2_foundation::{
    NSData, NSDate, NSDefaultRunLoopMode, NSMutableDictionary, NSNumber, NSRunLoop, NSString,
    NSSize,
};
use objc2_media_player::{
    MPMediaItemArtwork, MPMediaItemPropertyAlbumArtist, MPMediaItemPropertyAlbumTitle,
    MPMediaItemPropertyArtist, MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration,
    MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingInfoMediaType,
    MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyExternalContentIdentifier,
    MPNowPlayingInfoPropertyIsLiveStream, MPNowPlayingInfoPropertyMediaType,
    MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState, MPRemoteCommandCenter,
    MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};
use std::io::Read;
use std::ptr::NonNull;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Album shown under the track in Control Center; ytui has no album concept.
const ALBUM: &str = "YouTube";

pub struct Session {
    center: Retained<MPNowPlayingInfoCenter>,
    /// Kept alive for the lifetime of the session: dropping the command
    /// center would unregister the handlers.
    _commands: Retained<MPRemoteCommandCenter>,
    /// The opaque targets returned by `addTargetWithHandler`.
    _targets: Vec<Retained<AnyObject>>,
    current: Option<Now>,
    /// URL whose artwork is loaded or being loaded.
    art_url: String,
    artwork: Option<Retained<MPMediaItemArtwork>>,
    art_tx: Sender<(String, Option<Vec<u8>>)>,
    art_rx: Receiver<(String, Option<Vec<u8>>)>,
    stopped: bool,
}

impl Session {
    pub fn new(cmd_tx: Sender<Cmd>) -> Self {
        let center = unsafe { MPNowPlayingInfoCenter::defaultCenter() };
        let commands = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
        let cmd = Arc::new(Mutex::new(cmd_tx));
        let mut targets = Vec::new();
        register(&commands, &cmd, &mut targets);
        // A plain binary is not an app bundle, so macOS has to be told that it
        // is allowed to own the system-wide Now Playing slot. WebKit does the
        // same for its media helper processes.
        can_be_now_playing(true);
        let (art_tx, art_rx) = mpsc::channel();
        Self {
            center,
            _commands: commands,
            _targets: targets,
            current: None,
            art_url: String::new(),
            artwork: None,
            art_tx,
            art_rx,
            stopped: false,
        }
    }

    pub fn show(&mut self, now: Now) {
        self.stopped = false;
        if now.url != self.art_url {
            self.art_url = now.url.clone();
            self.artwork = None;
            if let Some(id) = video_id(&now.url) {
                let tx = self.art_tx.clone();
                let url = now.url.clone();
                let id = id.to_string();
                thread::spawn(move || {
                    let bytes = fetch_art(&id);
                    let _ = tx.send((url, bytes));
                });
            }
        }
        self.current = Some(now);
        self.publish();
    }

    /// Let Cocoa deliver pending remote-command events. Called once per frame.
    pub fn pump(&mut self) {
        while let Ok((url, bytes)) = self.art_rx.try_recv() {
            if url != self.art_url {
                continue;
            }
            if let Some(bytes) = bytes {
                if let Some(art) = artwork_from(&bytes) {
                    self.artwork = Some(art);
                    self.publish();
                }
            }
        }
        autoreleasepool(|_| unsafe {
            let _ = NSRunLoop::currentRunLoop()
                .runMode_beforeDate(NSDefaultRunLoopMode, &NSDate::distantPast());
        });
    }

    pub fn shutdown(&mut self) {
        self.clear();
        can_be_now_playing(false);
    }

    fn publish(&mut self) {
        let Some(now) = &self.current else { return };
        if now.title.is_empty() || matches!(now.status, Status::Stopped | Status::Error) {
            self.clear();
            return;
        }

        unsafe {
            let dict: Retained<NSMutableDictionary<NSString, AnyObject>> =
                NSMutableDictionary::new();
            let title = NSString::from_str(&now.title);
            let artist = NSString::from_str(&now.channel);
            let album = NSString::from_str(ALBUM);
            let external = NSString::from_str(&now.url);
            let duration = NSNumber::new_f64(now.dur);
            let elapsed = NSNumber::new_f64(now.pos);
            let playing = now.status == Status::Playing;
            let rate = NSNumber::new_f64(if playing { 1.0 } else { 0.0 });
            let media_type = NSNumber::new_u64(MPNowPlayingInfoMediaType::Audio.0 as u64);
            let live = NSNumber::new_bool(false);

            let _: () = msg_send![&*dict, setObject: &*title, forKey: MPMediaItemPropertyTitle];
            let _: () = msg_send![&*dict, setObject: &*artist, forKey: MPMediaItemPropertyArtist];
            let _: () = msg_send![&*dict, setObject: &*artist, forKey: MPMediaItemPropertyAlbumArtist];
            let _: () = msg_send![&*dict, setObject: &*album, forKey: MPMediaItemPropertyAlbumTitle];
            let _: () = msg_send![&*dict, setObject: &*duration, forKey: MPMediaItemPropertyPlaybackDuration];
            let _: () = msg_send![&*dict, setObject: &*elapsed, forKey: MPNowPlayingInfoPropertyElapsedPlaybackTime];
            let _: () = msg_send![&*dict, setObject: &*rate, forKey: MPNowPlayingInfoPropertyPlaybackRate];
            let _: () = msg_send![&*dict, setObject: &*media_type, forKey: MPNowPlayingInfoPropertyMediaType];
            let _: () = msg_send![&*dict, setObject: &*live, forKey: MPNowPlayingInfoPropertyIsLiveStream];
            // The Touch Bar/Control Center use this to associate artwork and
            // metadata with the same remote content item.
            let _: () = msg_send![&*dict, setObject: &*external, forKey: MPNowPlayingInfoPropertyExternalContentIdentifier];
            if let Some(art) = &self.artwork {
                let _: () = msg_send![&*dict, setObject: &**art, forKey: MPMediaItemPropertyArtwork];
            }

            let info: Retained<objc2_foundation::NSDictionary<NSString, AnyObject>> =
                dict.clone().into_super();
            self.center.setNowPlayingInfo(Some(&info));
            self.center.setPlaybackState(if playing {
                MPNowPlayingPlaybackState::Playing
            } else {
                MPNowPlayingPlaybackState::Paused
            });
        }
    }

    fn clear(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        self.current = None;
        self.art_url.clear();
        self.artwork = None;
        unsafe {
            self.center.setNowPlayingInfo(None);
            self.center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
        }
    }
}

fn register(
    cc: &MPRemoteCommandCenter,
    cmd: &Arc<Mutex<Sender<Cmd>>>,
    keep: &mut Vec<Retained<AnyObject>>,
) {
    macro_rules! wire {
        ($get:ident, $send:expr) => {{
            let tx = Arc::clone(cmd);
            let handler: RcBlock<
                dyn Fn(NonNull<MPRemoteCommandEvent>) -> MPRemoteCommandHandlerStatus,
            > = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                if let Ok(tx) = tx.lock() {
                    let _ = tx.send($send);
                }
                MPRemoteCommandHandlerStatus::Success
            });
            unsafe {
                let command = cc.$get();
                command.setEnabled(true);
                keep.push(command.addTargetWithHandler(&handler));
            }
        }};
    }

    wire!(playCommand, Cmd::Resume);
    wire!(pauseCommand, Cmd::Pause);
    wire!(togglePlayPauseCommand, Cmd::Toggle);
    wire!(nextTrackCommand, Cmd::Next);
    wire!(previousTrackCommand, Cmd::Prev);
    wire!(stopCommand, Cmd::Stop);

    // Dragging the Control Center scrubber sends a position event instead of
    // a button press.
    let tx = Arc::clone(cmd);
    let handler: RcBlock<
        dyn Fn(NonNull<MPRemoteCommandEvent>) -> MPRemoteCommandHandlerStatus,
    > = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
        let secs: f64 = unsafe { msg_send![event.as_ptr(), positionTime] };
        if secs.is_finite() && secs >= 0.0 {
            if let Ok(tx) = tx.lock() {
                let _ = tx.send(Cmd::SeekAbs(secs));
            }
        }
        MPRemoteCommandHandlerStatus::Success
    });
    unsafe {
        let command = cc.changePlaybackPositionCommand();
        command.setEnabled(true);
        keep.push(command.addTargetWithHandler(&handler));
    }
}

/// Private MediaRemote switch that lets a non-bundled process own the Now
/// Playing slot. Best-effort: if the symbol ever disappears, ytui still plays.
fn can_be_now_playing(can: bool) {
    unsafe {
        let path = c"/System/Library/PrivateFrameworks/MediaRemote.framework/MediaRemote";
        let handle = libc::dlopen(path.as_ptr(), libc::RTLD_NOW);
        if handle.is_null() {
            return;
        }
        let sym = libc::dlsym(handle, c"MRMediaRemoteSetCanBeNowPlayingApplication".as_ptr());
        if sym.is_null() {
            libc::dlclose(handle);
            return;
        }
        let set: unsafe extern "C" fn(u8) = std::mem::transmute(sym);
        set(u8::from(can));
        // The framework stays loaded for the lifetime of the process.
    }
}

fn artwork_from(data: &[u8]) -> Option<Retained<MPMediaItemArtwork>> {
    unsafe {
        let data = NSData::dataWithBytes_length(data.as_ptr().cast(), data.len());
        let image = NSImage::initWithData(NSImage::alloc(), &data)?;
        let size = image.size();
        // The block owns the image for as long as the artwork object lives,
        // which is what lets the request handler hand out a bare pointer.
        let image = image.clone();
        let handler = RcBlock::new(move |_requested: NSSize| -> NonNull<NSImage> {
            NonNull::from(image.as_ref())
        });
        Some(MPMediaItemArtwork::initWithBoundsSize_requestHandler(
            MPMediaItemArtwork::alloc(),
            size,
            &handler,
        ))
    }
}

/// YouTube thumbnails are predictable from the video id, which is already in
/// the watch URL the queue stores.
fn video_id(url: &str) -> Option<&str> {
    let id = match url.split("v=").nth(1) {
        Some(rest) => rest.split(['&', '#']).next()?,
        None => url
            .split("youtu.be/")
            .nth(1)?
            .split(['?', '&', '#'])
            .next()?,
    };
    (!id.is_empty()).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::video_id;

    #[test]
    fn watch_urls_yield_the_video_id() {
        assert_eq!(
            video_id("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RDAMVM"),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(
            video_id("https://youtu.be/dQw4w9WgXcQ?t=10"),
            Some("dQw4w9WgXcQ")
        );
        assert_eq!(video_id("https://example.com/stream"), None);
        assert_eq!(video_id(""), None);
    }
}

fn fetch_art(id: &str) -> Option<Vec<u8>> {
    let url = format!("https://i.ytimg.com/vi/{id}/hqdefault.jpg");
    let resp = ureq::get(&url)
        .timeout(Duration::from_secs(4))
        .call()
        .ok()?;
    let mut buf = Vec::with_capacity(32 * 1024);
    resp.into_reader()
        .take(2 * 1024 * 1024)
        .read_to_end(&mut buf)
        .ok()?;
    (!buf.is_empty()).then_some(buf)
}
