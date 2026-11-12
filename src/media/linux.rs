//! Linux media session selection: MPRIS normally, the Windows SMTC bridge or
//! an X11 key grab when ytui runs inside WSL.

use super::wsl;
use super::x11;
use super::{native, Event, NowPlaying};
use std::sync::mpsc::Sender;

pub struct Backend {
    inner: Inner,
}

enum Inner {
    Native(native::Backend),
    Bridge(wsl::Backend),
    X11(x11::Backend),
}

impl Backend {
    pub fn start(events: Sender<Event>) -> Option<Self> {
        // WSLg has no media session that sees the laptop's media keys. Prefer
        // the native Windows build over a pipe; if interop is unavailable,
        // WSLg's X server can still deliver the keys to an X grab.
        if is_wsl() {
            if let Some(bridge) = wsl::Backend::start(events.clone()) {
                return Some(Self {
                    inner: Inner::Bridge(bridge),
                });
            }
            if let Some(grab) = x11::Backend::start(events.clone()) {
                return Some(Self {
                    inner: Inner::X11(grab),
                });
            }
        }
        // On a normal desktop the shell owns the keys and drives MPRIS. If
        // there is no session bus, fall back to grabbing the keys ourselves.
        match native::Backend::start(events.clone()) {
            Some(native) => Some(Self {
                inner: Inner::Native(native),
            }),
            None => x11::Backend::start(events).map(|grab| Self {
                inner: Inner::X11(grab),
            }),
        }
    }

    pub fn update(&mut self, now: NowPlaying) {
        match &mut self.inner {
            Inner::Native(native) => native.update(now),
            Inner::Bridge(bridge) => bridge.update(now),
            Inner::X11(grab) => grab.update(now),
        }
    }

    pub fn mode(&self) -> String {
        match &self.inner {
            Inner::Native(native) => native.mode(),
            Inner::Bridge(bridge) => bridge.mode(),
            Inner::X11(grab) => grab.mode(),
        }
    }
}

pub fn is_wsl() -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_some() {
        return true;
    }
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|release| release.to_ascii_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

/// One line for `ytui doctor`.
pub fn status() -> String {
    if is_wsl() {
        return match wsl::find_bridge() {
            Some(path) => format!(
                "WSL bridge via {} (steers the native Windows media session)",
                path.display()
            ),
            None if std::env::var_os("DISPLAY").is_some() => {
                "WSLg X11 media keys (no ytui.exe found for the Windows bridge)".to_string()
            }
            None => "WSL: no ytui.exe on PATH and no X display, so media keys are unavailable"
                .to_string(),
        };
    }
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some() {
        "MPRIS (desktop media keys, playerctl, lock screen)".to_string()
    } else if std::env::var_os("DISPLAY").is_some() {
        "X11 media keys (no D-Bus session; grabbing the XF86 keys directly)".to_string()
    } else {
        "no media session found (media keys may not work)".to_string()
    }
}
