//! macOS Now Playing, plus the AppKit run loop that macOS requires for it.
//!
//! Souvlaki registers with `MPRemoteCommandCenter` and publishes through
//! `MPNowPlayingInfoCenter`, but a command-line process has no application run
//! loop. MediaPlayer delivers the handlers through that loop, so without one
//! the buttons are silently ignored. ytui creates a hidden accessory
//! `NSApplication` (and a 1x1 off-screen window) once, then gives AppKit a few
//! microseconds on every TUI iteration in [`pump`].

use super::{native, Event, NowPlaying};
use core_graphics::geometry::{CGPoint, CGRect, CGSize};
use objc::runtime::{Object, BOOL, NO, YES};
use objc::{class, msg_send, sel, sel_impl};
use std::ffi::CString;
use std::sync::mpsc::Sender;

type Id = *mut Object;

pub struct Backend {
    native: native::Backend,
    // AppKit requires these to live for the process; ytui exits as a whole, so
    // deliberately not releasing them avoids a use-after-free in MediaPlayer.
    _app: Id,
    _window: Id,
}

impl Backend {
    pub fn start(events: Sender<Event>) -> Option<Self> {
        let (app, window) = unsafe { app_and_window()? };
        let native = native::Backend::start(events)?;
        Some(Self {
            native,
            _app: app,
            _window: window,
        })
    }

    pub fn update(&mut self, now: NowPlaying) {
        self.native.update(now);
    }

    pub fn mode(&self) -> String {
        "macOS Now Playing".to_string()
    }
}

unsafe fn app_and_window() -> Option<(Id, Id)> {
    let app: Id = unsafe { msg_send![class!(NSApplication), sharedApplication] };
    if app.is_null() {
        return None;
    }
    // Accessory keeps ytui out of the Dock and the app switcher; it is a
    // terminal player, not a windowed app.
    let _: BOOL = unsafe { msg_send![app, setActivationPolicy: 1isize] };
    let _: () = unsafe { msg_send![app, finishLaunching] };

    let rect = CGRect::new(&CGPoint::new(0.0, 0.0), &CGSize::new(1.0, 1.0));
    let window: Id = unsafe { msg_send![class!(NSWindow), alloc] };
    let window: Id = unsafe {
        msg_send![window,
            initWithContentRect: rect
            styleMask: 0u64
            backing: 2u64
            defer: NO]
    };
    if window.is_null() {
        return None;
    }
    let _: () = unsafe { msg_send![window, setReleasedWhenClosed: NO] };
    Some((app, window))
}

/// Run the application's pending events without blocking the TUI. AppKit does
/// not need ytui to be frontmost; it needs the loop to run so MediaPlayer can
/// deliver the registered command handlers.
pub fn pump() {
    unsafe {
        let app: Id = msg_send![class!(NSApplication), sharedApplication];
        if app.is_null() {
            return;
        }
        // Cocoa autoreleases event objects; a command-line process has no
        // pool of its own, so make one per pump and drain it before returning.
        let pool: Id = msg_send![class!(NSAutoreleasePool), new];
        let date: Id = msg_send![class!(NSDate), dateWithTimeIntervalSinceNow: 0.0f64];
        let mode = ns_string("kCFRunLoopDefaultMode");
        let run_loop: Id = msg_send![class!(NSRunLoop), currentRunLoop];
        let _: BOOL = msg_send![run_loop, runMode: mode beforeDate: date];
        loop {
            let event: Id = msg_send![
                app,
                nextEventMatchingMask: u64::MAX
                untilDate: date
                inMode: mode
                dequeue: YES
            ];
            if event.is_null() {
                break;
            }
            let _: () = msg_send![app, sendEvent: event];
        }
        let _: () = msg_send![pool, drain];
    }
}

unsafe fn ns_string(value: &str) -> Id {
    let text = CString::new(value).expect("run-loop mode has no interior NUL");
    let string: Id = unsafe { msg_send![class!(NSString), stringWithUTF8String: text.as_ptr()] };
    string
}
