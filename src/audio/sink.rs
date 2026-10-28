//! cpal output: one long-lived stereo f32 stream at the track's sample rate.
//!
//! The decoder thread fills a bounded ring buffer and the audio callback drains
//! it. The callback holds the mutex for two `copy_from_slice` calls and never
//! allocates.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub struct Sink {
    name: String,
    buf: Arc<Mutex<VecDeque<f32>>>,
    played: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
    /// Set while a track is being opened and the ring is still filling. The
    /// callback then outputs silence without counting it as played time.
    hold: Arc<AtomicBool>,
    volume: Arc<AtomicI32>,
    muted: Arc<AtomicBool>,
    dev_err: Arc<Mutex<Option<String>>>,
    capacity: usize,
    rate: u32,
    _stream: cpal::Stream,
}

/// Size of the ring buffer in interleaved f32 samples.
///
/// The engine only refills the sink while it holds less than `buffer_ms` of
/// audio, then pushes whole decoded packets (up to ~2.3k frames each). The
/// capacity therefore has to be strictly larger than the refill threshold,
/// otherwise every top-up would overflow and silently throw the tail of the
/// packet away, which makes the music skip and run faster than real time. Give
/// it a full extra second of headroom.
fn ring_capacity(buffer_ms: u32, rate: u32) -> usize {
    ((rate as usize * 2 * (buffer_ms.max(200) as usize + 1000)) / 1000).max(8192)
}

/// True inside WSL, where ALSA has no sound card at all: WSLg only exposes a
/// PulseAudio socket.
fn is_wsl() -> bool {
    Path::new("/mnt/wslg/PulseServer").exists()
        || std::fs::read_to_string("/proc/version")
            .map(|v| v.to_ascii_lowercase().contains("microsoft"))
            .unwrap_or(false)
}

/// True when PulseAudio is the intended output. On WSL that is the only
/// option; elsewhere `PULSE_SERVER` means the user routed ALSA there on
/// purpose, so the plugin is still the right first guess.
fn prefers_pulse() -> bool {
    std::env::var_os("PULSE_SERVER").is_some() || is_wsl()
}

/// Where a device name sits in the list of devices to try.
///
/// `default` is the name ALSA resolves from its system configuration. On WSL
/// it points at hardware card 0, which does not exist, so the plugin-backed
/// `pulse` PCM has to come first there.
fn rank(name: &str, prefer_pulse: bool) -> u8 {
    match (prefer_pulse, name) {
        (true, "pulse") => 0,
        (true, "pipewire") => 1,
        (true, "default") => 2,
        (false, "default") => 0,
        (false, "pulse") => 1,
        (false, "pipewire") => 2,
        _ => 3,
    }
}

/// Output devices to try, best candidate first.
///
/// `HostTrait` has no open-by-name, but the ALSA backend enumerates every PCM
/// from the configuration, including the `pulse` PCM that
/// `libasound2-plugins` adds. Opening the candidates in turn therefore finds
/// the plugin without writing `~/.asoundrc`.
fn candidates() -> Vec<(String, cpal::Device)> {
    let host = cpal::default_host();
    let prefer_pulse = prefers_pulse();
    let mut devices: Vec<(String, cpal::Device)> = Vec::new();

    if let Some(device) = host.default_output_device() {
        devices.push(("default".to_string(), device));
    }

    // On a healthy machine the default PCM is the answer. Enumerating opens
    // every PCM in the configuration (playback *and* capture), so only do it
    // when the default is not expected to work: on WSL, or when it cannot even
    // be queried. This failed query is also what says `pulse` is needed.
    let default_works = !prefer_pulse
        && devices
            .first()
            .is_some_and(|(_, device)| device.default_output_config().is_ok());

    if !default_works {
        if let Ok(list) = host.output_devices() {
            for device in list {
                let name = match device.name() {
                    Ok(name) => name,
                    Err(_) => continue,
                };
                // The default device is constructed above; do not try it twice.
                if devices.iter().any(|(seen, _)| *seen == name) {
                    continue;
                }
                devices.push((name, device));
            }
        }
    }

    // A stable sort keeps the ALSA hint order for everything with equal rank.
    devices.sort_by_key(|(name, _)| rank(name, prefer_pulse));
    devices
}

/// The useful tail of cpal's ALSA error, e.g. `No such file or directory (2)`.
fn brief(err: &str) -> String {
    let tail = err
        .rsplit("failed with error ")
        .next()
        .unwrap_or(err)
        .trim();
    tail.trim_matches('\'').to_string()
}

/// A message the player can show on one line. The raw cpal/ALSA sentence is
/// several times wider than a terminal and hides the thing that can be fixed.
fn open_error(tries: &[(String, String)]) -> String {
    let first = tries
        .first()
        .map(|(_, err)| brief(err))
        .unwrap_or_else(|| "no output device".to_string());
    let pulse_tried = tries
        .iter()
        .any(|(name, _)| name == "pulse" || name == "pipewire");

    if is_wsl() && !pulse_tried {
        "no WSL sound card: install libasound2-plugins (sudo apt-get install -y libasound2-plugins)"
            .to_string()
    } else if prefers_pulse() && pulse_tried {
        format!("cannot reach PulseAudio: {first} (check PULSE_SERVER)")
    } else {
        format!("cannot open audio device: {first}")
    }
}

/// ALSA prints its own diagnostics (`cannot find card '0'`) to stderr while
/// probing. Inside the TUI that lands in the middle of the frame, so silence
/// stderr for the duration of an open attempt; `open_error` reports the
/// failure instead.
#[cfg(target_os = "linux")]
struct QuietStderr {
    saved: libc::c_int,
}

#[cfg(target_os = "linux")]
impl QuietStderr {
    fn new() -> Option<Self> {
        unsafe {
            let saved = libc::dup(libc::STDERR_FILENO);
            if saved < 0 {
                return None;
            }
            let null = libc::open(
                b"/dev/null\0".as_ptr() as *const libc::c_char,
                libc::O_WRONLY,
            );
            if null < 0 {
                libc::close(saved);
                return None;
            }
            let switched = libc::dup2(null, libc::STDERR_FILENO);
            libc::close(null);
            if switched < 0 {
                libc::close(saved);
                return None;
            }
            Some(Self { saved })
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for QuietStderr {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.saved, libc::STDERR_FILENO);
            libc::close(self.saved);
        }
    }
}

#[cfg(not(target_os = "linux"))]
struct QuietStderr;

#[cfg(not(target_os = "linux"))]
impl QuietStderr {
    fn new() -> Option<Self> {
        None
    }
}

impl Sink {
    /// Preferred rate of the output device, if it can be queried.
    pub fn default_rate() -> Option<u32> {
        let _quiet = QuietStderr::new();
        for (_, device) in candidates() {
            if let Ok(config) = device.default_output_config() {
                return Some(config.sample_rate().0);
            }
        }
        None
    }

    pub fn new(rate: u32, buffer_ms: u32) -> Result<Self, String> {
        let _quiet = QuietStderr::new();

        let config = cpal::StreamConfig {
            channels: 2,
            sample_rate: cpal::SampleRate(rate),
            buffer_size: cpal::BufferSize::Default,
        };

        let capacity = ring_capacity(buffer_ms, rate);
        let buf = Arc::new(Mutex::new(VecDeque::<f32>::with_capacity(capacity)));
        let played = Arc::new(AtomicU64::new(0));
        let paused = Arc::new(AtomicBool::new(false));
        let hold = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicI32::new(100));
        let muted = Arc::new(AtomicBool::new(false));
        let dev_err = Arc::new(Mutex::new(None));

        let mut stream = None;
        let mut name = String::new();
        let mut tries: Vec<(String, String)> = Vec::new();

        for (device_name, device) in candidates() {
            let cb_buf = Arc::clone(&buf);
            let cb_played = Arc::clone(&played);
            let cb_paused = Arc::clone(&paused);
            let cb_hold = Arc::clone(&hold);
            let cb_vol = Arc::clone(&volume);
            let cb_muted = Arc::clone(&muted);
            let cb_err = Arc::clone(&dev_err);

            let built = device.build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    if cb_paused.load(Ordering::Relaxed) || cb_hold.load(Ordering::Relaxed) {
                        data.fill(0.0);
                        return;
                    }
                    let gain = if cb_muted.load(Ordering::Relaxed) {
                        0.0
                    } else {
                        cb_vol.load(Ordering::Relaxed) as f32 / 100.0
                    };
                    let mut q = cb_buf.lock().unwrap_or_else(|e| e.into_inner());
                    let (head, tail) = q.as_slices();
                    let n1 = head.len().min(data.len());
                    data[..n1].copy_from_slice(&head[..n1]);
                    let n2 = (data.len() - n1).min(tail.len());
                    data[n1..n1 + n2].copy_from_slice(&tail[..n2]);
                    data[n1 + n2..].fill(0.0);
                    q.drain(..n1 + n2);
                    drop(q);
                    if (gain - 1.0).abs() > f32::EPSILON {
                        for s in data.iter_mut() {
                            *s = (*s * gain).clamp(-1.0, 1.0);
                        }
                    }
                    cb_played.fetch_add((data.len() / 2) as u64, Ordering::Relaxed);
                },
                move |e| {
                    *cb_err.lock().unwrap_or_else(|x| x.into_inner()) = Some(e.to_string());
                },
                None,
            );

            match built {
                Ok(s) => {
                    stream = Some(s);
                    name = device_name;
                    break;
                }
                Err(e) => tries.push((device_name, e.to_string())),
            }
        }

        let stream = stream.ok_or_else(|| open_error(&tries))?;

        stream
            .play()
            .map_err(|e| format!("cannot start audio device: {}", brief(&e.to_string())))?;

        Ok(Self {
            name,
            buf,
            played,
            paused,
            hold,
            volume,
            muted,
            dev_err,
            capacity,
            rate,
            _stream: stream,
        })
    }

    /// Halt the output while the decoder fills the ring, without touching the
    /// user's pause state and without counting the silence as playback.
    pub fn set_hold(&self, on: bool) {
        self.hold.store(on, Ordering::Relaxed);
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    /// Name of the PCM that was opened, e.g. `pulse` on WSL.
    pub fn device_name(&self) -> &str {
        &self.name
    }

    pub fn buffered_frames(&self) -> usize {
        let q = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        q.len() / 2
    }

    pub fn push(&self, samples: &[f32]) {
        let mut q = self.buf.lock().unwrap_or_else(|e| e.into_inner());
        for &s in samples {
            if q.len() >= self.capacity {
                break;
            }
            // AAC can overshoot; keep the device from wrapping integers.
            q.push_back(s.clamp(-1.0, 1.0));
        }
    }

    pub fn clear(&self) {
        self.buf.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    pub fn played_frames(&self) -> u64 {
        self.played.load(Ordering::Relaxed)
    }

    pub fn set_played_frames(&self, frames: u64) {
        self.played.store(frames, Ordering::Relaxed);
    }

    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    pub fn set_volume(&self, percent: i64) {
        self.volume.store(percent as i32, Ordering::Relaxed);
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Relaxed);
    }

    pub fn take_error(&self) -> Option<String> {
        self.dev_err
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the "plays fast and skips" bug: the ring must be
    /// able to hold the refill threshold plus a whole decoded packet.
    #[test]
    fn ring_has_headroom_over_refill_threshold() {
        for rate in [22_050u32, 44_100, 48_000, 96_000] {
            for buffer_ms in [200u32, 500, 1000, 2000, 5000] {
                let target = (rate as usize * buffer_ms as usize / 1000).max(4096);
                let capacity = ring_capacity(buffer_ms, rate) / 2;
                assert!(
                    capacity > target + rate as usize / 10,
                    "rate={rate} buffer_ms={buffer_ms}: capacity {capacity} too close to target {target}"
                );
            }
        }
    }

    /// WSL has no card 0, so `default` fails there and the PulseAudio PCM has
    /// to be tried first. On a normal machine the hardware default wins.
    #[test]
    fn pulse_comes_first_only_where_it_is_the_only_option() {
        assert!(rank("pulse", true) < rank("default", true));
        assert!(rank("default", false) < rank("pulse", false));
        assert!(rank("pipewire", true) < rank("default", true));
        assert!(rank("hw:CARD=Intel", true) > rank("pulse", true));
    }

    /// The player status line is one row wide, so the three-line cpal error
    /// has to be reduced to its actionable tail.
    #[test]
    fn backend_errors_are_shortened() {
        let err = "A backend-specific error has occurred: ALSA function \
                   'snd_pcm_open' failed with error 'No such file or directory (2)'";
        assert_eq!(brief(err), "No such file or directory (2)");
        assert_eq!(brief("plain error"), "plain error");
    }
}
