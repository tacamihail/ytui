//! cpal output: one long-lived stereo f32 stream at the track's sample rate.
//!
//! The decoder thread fills a bounded ring buffer and the audio callback drains
//! it. The callback holds the mutex for two `copy_from_slice` calls and never
//! allocates.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

pub struct Sink {
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

impl Sink {
    /// Preferred rate of the default output device, if it can be queried.
    pub fn default_rate() -> Option<u32> {
        let host = cpal::default_host();
        let device = host.default_output_device()?;
        device
            .default_output_config()
            .ok()
            .map(|c| c.sample_rate().0)
    }

    pub fn new(rate: u32, buffer_ms: u32) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no audio output device found".to_string())?;

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

        let cb_buf = Arc::clone(&buf);
        let cb_played = Arc::clone(&played);
        let cb_paused = Arc::clone(&paused);
        let cb_hold = Arc::clone(&hold);
        let cb_vol = Arc::clone(&volume);
        let cb_muted = Arc::clone(&muted);
        let cb_err = Arc::clone(&dev_err);

        let stream = device
            .build_output_stream(
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
            )
            .map_err(|e| format!("cannot open audio device: {e}"))?;

        stream
            .play()
            .map_err(|e| format!("cannot start audio device: {e}"))?;

        Ok(Self {
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
}
