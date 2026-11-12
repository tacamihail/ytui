//! X11 media-key fallback for WSLg and bare X sessions.
//!
//! MPRIS depends on a desktop shell that listens for the keys and forwards
//! them. WSLg has an X server but no such shell, so nothing is listening even
//! though the keys reach X. Grabbing the XF86 keys on the root window makes
//! the X server deliver them straight to ytui instead.
//!
//! The grabs are best-effort: a real desktop usually owns these combinations
//! already, and a failed grab (BadAccess) simply leaves that key to the
//! desktop, which drives the MPRIS session instead.

use super::Event;
use std::collections::HashMap;
use std::sync::mpsc::{self, Sender, SyncSender};
use std::thread;
use std::time::Duration;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, GrabMode, ModMask};

const XF86_AUDIO_MUTE: u32 = 0x1008_FF12;
const XF86_AUDIO_PLAY: u32 = 0x1008_FF14;
const XF86_AUDIO_STOP: u32 = 0x1008_FF15;
const XF86_AUDIO_PREV: u32 = 0x1008_FF16;
const XF86_AUDIO_NEXT: u32 = 0x1008_FF17;
const XF86_AUDIO_PAUSE: u32 = 0x1008_FF31;

pub struct Backend {
    // The grabbing thread lives as long as the process. Dropping the handle
    // detaches it; the X server releases the grabs when the process exits.
    _thread: thread::JoinHandle<()>,
}

impl Backend {
    pub fn start(events: Sender<Event>) -> Option<Self> {
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let handle = thread::Builder::new()
            .name("ytui-media-x11".into())
            .spawn(move || run(events, ready_tx))
            .ok()?;
        match ready_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(true) => Some(Self { _thread: handle }),
            _ => None,
        }
    }

    /// X11 only carries key presses; track metadata still comes from the
    /// player itself, so there is nothing to publish here.
    pub fn update(&mut self, _now: super::NowPlaying) {}

    pub fn mode(&self) -> String {
        "X11 media-key grab".to_string()
    }
}

fn run(events: Sender<Event>, ready: SyncSender<bool>) {
    let Ok((connection, screen)) = x11rb::connect(None) else {
        let _ = ready.send(false);
        return;
    };
    let setup = connection.setup();
    let root = setup.roots[screen].root;
    let min_keycode = setup.min_keycode;
    let max_keycode = setup.max_keycode;
    let count = u16::from(max_keycode) - u16::from(min_keycode) + 1;

    let mapping = match connection
        .get_keyboard_mapping(min_keycode, count as u8)
        .ok()
        .and_then(|cookie| cookie.reply().ok())
    {
        Some(mapping) => mapping,
        None => {
            let _ = ready.send(false);
            return;
        }
    };
    let stride = usize::from(mapping.keysyms_per_keycode.max(1));
    let mut keys: HashMap<u8, Event> = HashMap::new();
    for (index, symbols) in mapping.keysyms.chunks(stride).enumerate() {
        let keycode = min_keycode.wrapping_add(index as u8);
        for symbol in symbols {
            if let Some(event) = key_event(*symbol) {
                keys.insert(keycode, event);
                break;
            }
        }
    }
    if keys.is_empty() {
        let _ = ready.send(false);
        return;
    }

    // Caps Lock and Num Lock change the modifier bits, so grab each key with
    // and without them. Shift is deliberately omitted: media keys do not use
    // it, and grabbing it would swallow Shift+Play from other apps.
    let lock = u16::from(ModMask::LOCK);
    let num_lock = u16::from(ModMask::M2);
    let modifiers = [0, lock, num_lock, lock | num_lock];
    let mut grabbed = 0usize;
    for &keycode in keys.keys() {
        for &modifier in &modifiers {
            if connection
                .grab_key(
                    false,
                    root,
                    modifier.into(),
                    keycode,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )
                .ok()
                .and_then(|cookie| cookie.check().ok())
                .is_some()
            {
                grabbed += 1;
            }
        }
    }
    let _ = connection.flush();
    if grabbed == 0 {
        let _ = ready.send(false);
        return;
    }
    if ready.send(true).is_err() {
        return;
    }

    while let Ok(event) = connection.wait_for_event() {
        if let x11rb::protocol::Event::KeyPress(press) = event {
            if let Some(event) = keys.get(&press.detail) {
                if events.send(*event).is_err() {
                    return;
                }
            }
        }
    }
}

fn key_event(symbol: u32) -> Option<Event> {
    match symbol {
        XF86_AUDIO_PLAY => Some(Event::Toggle),
        XF86_AUDIO_PAUSE => Some(Event::Pause),
        XF86_AUDIO_STOP => Some(Event::Stop),
        XF86_AUDIO_PREV => Some(Event::Prev),
        XF86_AUDIO_NEXT => Some(Event::Next),
        XF86_AUDIO_MUTE => Some(Event::Mute),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use x11rb::protocol::xtest::ConnectionExt as _;

    /// End-to-end check of the grab: start the backend, synthesize the play
    /// key through XTEST and expect a Toggle. Ignored normally because it needs
    /// a live X server; run it under WSLg with
    /// `cargo test x11 -- --ignored` to verify the WSL fallback.
    #[test]
    #[ignore = "needs an X server with the XTEST extension"]
    fn the_play_key_becomes_a_toggle() {
        let (tx, rx) = mpsc::channel();
        let _backend = Backend::start(tx).expect("X11 media-key grab");
        let (connection, screen) = x11rb::connect(None).expect("X server");
        let setup = connection.setup();
        let count = setup.max_keycode - setup.min_keycode + 1;
        let mapping = connection
            .get_keyboard_mapping(setup.min_keycode, count)
            .unwrap()
            .reply()
            .unwrap();
        let stride = usize::from(mapping.keysyms_per_keycode.max(1));
        let keycode = mapping
            .keysyms
            .chunks(stride)
            .position(|symbols| symbols.contains(&XF86_AUDIO_PLAY))
            .map(|index| setup.min_keycode.wrapping_add(index as u8));
        let Some(keycode) = keycode else {
            return;
        };
        let root = setup.roots[screen].root;
        connection
            .xtest_fake_input(2, keycode, 0, root, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
        connection
            .xtest_fake_input(3, keycode, 0, root, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
        connection.flush().unwrap();
        assert_eq!(rx.recv_timeout(Duration::from_secs(2)), Ok(Event::Toggle));
    }
}
