//! Windows System Media Transport Controls (SMTC).
//!
//! SMTC is a WinRT apartment/thread-affine API with a window handle, so all of
//! it lives on a dedicated thread with its own hidden top-level window and a
//! message pump. The UI thread only pushes state into a channel and posts a
//! wake-up message, which keeps drawing from waiting on WinRT.

use super::{Event, NowPlaying, Playback};
use souvlaki::{
    MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig,
    SeekDirection,
};
use std::ffi::c_void;
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::thread;
use std::time::Duration;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows_sys::Win32::System::Console::GetConsoleWindow;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PeekMessageW,
    PostThreadMessageW, RegisterClassW, TranslateMessage, MSG, PM_NOREMOVE, WM_APP, WM_QUIT,
    WNDCLASSW, WS_OVERLAPPED,
};

/// Posted to the media thread after a state update lands in the channel.
const WAKE: u32 = WM_APP + 1;
const SEEK_STEP: f64 = 10.0;
const INIT_TIMEOUT: Duration = Duration::from_secs(2);

pub struct Backend {
    tx: Sender<NowPlaying>,
    thread_id: u32,
}

impl Backend {
    pub fn start(events: Sender<Event>) -> Option<Self> {
        let (tx, rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("ytui-media".into())
            .spawn(move || run_worker(rx, events, ready_tx))
            .ok()?;
        match ready_rx.recv_timeout(INIT_TIMEOUT) {
            Ok(Ok(thread_id)) => Some(Self { tx, thread_id }),
            _ => None,
        }
    }

    pub fn update(&mut self, now: NowPlaying) {
        if self.tx.send(now).is_ok() {
            unsafe {
                PostThreadMessageW(self.thread_id, WAKE, 0, 0);
            }
        }
    }

    pub fn mode(&self) -> String {
        "Windows SMTC".to_string()
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        unsafe {
            PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0);
        }
    }
}

fn run_worker(
    rx: Receiver<NowPlaying>,
    events: Sender<Event>,
    ready: SyncSender<Result<u32, String>>,
) {
    unsafe {
        CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32);
    }

    let hwnd = match unsafe { hidden_window() } {
        Some(hwnd) => hwnd,
        None => {
            let _ = ready.send(Err("could not create the SMTC window".into()));
            return;
        }
    };
    let mut controls = match create_controls(hwnd) {
        Ok(controls) => controls,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let event_tx = events;
    if let Err(e) = controls.attach(move |event| {
        if let Some(event) = map_event(event) {
            let _ = event_tx.send(event);
        }
    }) {
        let _ = ready.send(Err(format!("could not attach to SMTC: {e}")));
        return;
    }

    let thread_id = unsafe { GetCurrentThreadId() };
    // Make sure the thread message queue exists before the UI thread can post
    // its first update; CreateWindowExW normally does this, but the console
    // fallback does not create a window on this thread.
    unsafe {
        let mut peek: MSG = std::mem::zeroed();
        PeekMessageW(&mut peek, std::ptr::null_mut(), 0, 0, PM_NOREMOVE);
    }
    if ready.send(Ok(thread_id)).is_err() {
        return;
    }

    let mut meta: Option<Meta> = None;
    let mut message: MSG = unsafe { std::mem::zeroed() };
    loop {
        let result = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        if message.message == WAKE {
            while let Ok(now) = rx.try_recv() {
                apply(&mut controls, &mut meta, now);
            }
            continue;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}

/// Try a hidden top-level window first. SMTC wants an app window; the console's
/// window belongs to Windows Terminal or conhost, so borrowing it can register
/// the wrong app (or fail outright under a pseudoconsole).
unsafe fn hidden_window() -> Option<HWND> {
    let hinstance = unsafe { GetModuleHandleW(std::ptr::null()) };
    if hinstance.is_null() {
        return console_window();
    }
    let class: Vec<u16> = "ytui-media-window\0".encode_utf16().collect();
    let title: Vec<u16> = "ytui\0".encode_utf16().collect();

    let mut class_info: WNDCLASSW = unsafe { std::mem::zeroed() };
    class_info.lpfnWndProc = Some(DefWindowProcW);
    class_info.hInstance = hinstance;
    class_info.lpszClassName = class.as_ptr();
    unsafe {
        // Zero means the class may already exist in this process; creating the
        // window is what matters, so the return value is intentionally ignored.
        RegisterClassW(&class_info);
    }

    let hwnd = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            hinstance,
            std::ptr::null(),
        )
    };
    if hwnd.is_null() {
        console_window()
    } else {
        Some(hwnd)
    }
}

/// Last resort for hosts that reject windows with a zero size.
fn console_window() -> Option<HWND> {
    let hwnd = unsafe { GetConsoleWindow() };
    if hwnd.is_null() {
        None
    } else {
        Some(hwnd)
    }
}

fn create_controls(hwnd: HWND) -> Result<MediaControls, String> {
    let config = PlatformConfig {
        display_name: super::APP_NAME,
        dbus_name: super::APP_NAME,
        hwnd: Some(hwnd as *mut c_void),
    };
    match MediaControls::new(config) {
        Ok(controls) => Ok(controls),
        Err(first) => {
            // Some Windows builds refuse an invisible window for SMTC. The
            // console handle is a poor identity but better than no media keys.
            let console = console_window();
            if console.is_some() && console != Some(hwnd) {
                if let Ok(controls) = MediaControls::new(PlatformConfig {
                    display_name: super::APP_NAME,
                    dbus_name: super::APP_NAME,
                    hwnd: console.map(|h| h as *mut c_void),
                }) {
                    return Ok(controls);
                }
            }
            Err(format!("SMTC is unavailable: {first}"))
        }
    }
}

struct Meta {
    title: String,
    artist: String,
    duration: u64,
}

impl PartialEq for Meta {
    fn eq(&self, other: &Self) -> bool {
        self.title == other.title && self.artist == other.artist && self.duration == other.duration
    }
}

fn apply(controls: &mut MediaControls, meta: &mut Option<Meta>, now: NowPlaying) {
    let current = Meta {
        title: now.title,
        artist: now.artist,
        duration: now.duration.max(0.0) as u64,
    };
    if meta.as_ref() != Some(&current) {
        let duration = (current.duration > 0).then(|| Duration::from_secs(current.duration));
        let ok = controls
            .set_metadata(MediaMetadata {
                title: Some(&current.title),
                artist: Some(&current.artist),
                album: None,
                cover_url: None,
                duration,
            })
            .is_ok();
        if ok {
            *meta = Some(current);
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
    let _ = controls.set_playback(playback);
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
        _ => None,
    }
}

fn seek_amount(direction: SeekDirection, amount: f64) -> f64 {
    match direction {
        SeekDirection::Forward => amount,
        SeekDirection::Backward => -amount,
    }
}

// Keep the raw Windows event types referenced so the cfg-gated imports do not
// drift if the window procedure is ever customised.
#[allow(dead_code)]
type WndProc = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;
