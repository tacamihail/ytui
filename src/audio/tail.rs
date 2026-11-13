//! Finding the silence at the end of a track.
//!
//! The decoder never holds more than `buffer_ms` of audio, so the last seconds
//! cannot be inspected while playing without buffering them (10 s of PCM is
//! ~4.6 MB, which this player will not spend). Instead a short-lived scan runs
//! its own decoder over the final [`SPAN_SECS`] of the stream, keeps one RMS
//! number per [`BLOCK_SECS`] (~120 f32) and throws the PCM away. The cost is a
//! few milliseconds of CPU and one extra range request that playback will need
//! a few seconds later anyway.

use std::sync::atomic::{AtomicBool, Ordering};

use super::decode::Loaded;

/// Length of one RMS block. Long enough to average a note, short enough that
/// the cut lands within a tenth of a second of the silence.
pub const BLOCK_SECS: f64 = 0.1;
/// How much of the tail is examined. A quiet run has to reach the end of the
/// file, so this only needs to cover the longest run worth cutting.
pub const SPAN_SECS: f64 = 12.0;
/// A trailing run shorter than this is left alone: fades and applause would
/// otherwise be mistaken for dead air.
pub const MIN_QUIET_SECS: f64 = 7.0;
/// Absolute floor. Below this a block is silence whatever the track's level.
const FLOOR_DB: f32 = -45.0;
/// Relative gate, so a quiet recording (acoustic, ambient) is not truncated:
/// a block also has to sit this far below the track's own median.
const BELOW_MEDIAN_DB: f32 = -30.0;

/// Scans the tail of `url` and returns the absolute position where the trailing
/// silence starts, or `None` when the ending is not silent. Runs on its own
/// thread; `cancel` stops it after a seek or a track change.
pub fn scan(url: &str, duration: f64, chunk: usize, cancel: &AtomicBool) -> Option<f64> {
    if duration <= SPAN_SECS {
        return None;
    }
    let mut loaded = Loaded::open(url, chunk).ok()?;
    // Decode at the container rate: this buffer is thrown away, so resampling
    // it would only cost CPU.
    let rate = loaded.rate().max(1);
    loaded.set_target_rate(rate);
    let resumed = loaded.seek(duration - SPAN_SECS).ok()?;
    let block = ((f64::from(rate) * BLOCK_SECS).round() as usize).max(1);

    let mut levels = Vec::with_capacity((SPAN_SECS / BLOCK_SECS) as usize + 2);
    let mut sum = 0.0f64;
    let mut frames = 0usize;
    let mut tmp = Vec::with_capacity(16 * 1024);
    loop {
        if cancel.load(Ordering::Relaxed) {
            return None;
        }
        tmp.clear();
        if !loaded.next(&mut tmp).unwrap_or(false) {
            break;
        }
        let frames_in = tmp.len() / 2;
        for i in 0..frames_in {
            let mono = (tmp[i * 2] + tmp[i * 2 + 1]) * 0.5;
            sum += f64::from(mono * mono);
            frames += 1;
            if frames == block {
                levels.push((sum / frames as f64).sqrt() as f32);
                sum = 0.0;
                frames = 0;
            }
        }
    }
    if frames > 0 {
        levels.push((sum / frames as f64).sqrt() as f32);
    }

    quiet_cut(&levels).map(|offset| resumed + offset)
}

/// Where the trailing quiet run starts, in seconds from the first block, or
/// `None` when the ending is not quiet for long enough.
///
/// A block counts as quiet only when it is below the absolute floor *and* far
/// below the track's own level, so a song that is quiet throughout keeps its
/// ending.
pub fn quiet_cut(levels: &[f32]) -> Option<f64> {
    if levels.len() < 2 {
        return None;
    }
    let reference = loud_level(levels);
    let floor = 10f32.powf(FLOOR_DB / 20.0);
    let relative = reference * 10f32.powf(BELOW_MEDIAN_DB / 20.0);
    let limit = floor.min(relative);
    let run = levels.iter().rev().take_while(|level| **level < limit).count();
    let quiet = run as f64 * BLOCK_SECS;
    if quiet < MIN_QUIET_SECS {
        return None;
    }
    Some(levels.len() as f64 * BLOCK_SECS - quiet)
}

/// The track's own level, taken high enough that a long silent tail does not
/// drag it down: the 80th percentile of the scanned blocks, not the median.
///
/// With the median, a span that is mostly silence puts the reference at zero
/// and the relative gate can never pass — the very case this feature exists
/// for.
fn loud_level(levels: &[f32]) -> f32 {
    let mut sorted: Vec<f32> = levels.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sorted[(sorted.len() * 4) / 5]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A second of loud blocks, then `quiet_secs` of silence.
    fn track(loud: f32, quiet_secs: f64) -> Vec<f32> {
        let mut levels = vec![loud; 30];
        levels.extend(std::iter::repeat_n(0.0, (quiet_secs / BLOCK_SECS) as usize));
        levels
    }

    #[test]
    fn a_silent_tail_is_cut_where_it_starts() {
        let levels = track(0.3, 8.0);
        let cut = quiet_cut(&levels).expect("8 s of silence is a tail");
        assert!((cut - 3.0).abs() < 0.05, "cut at {cut}");
    }

    #[test]
    fn a_short_gap_before_the_end_is_not_a_tail() {
        // Fade-out or applause: seven seconds is the threshold.
        assert!(quiet_cut(&track(0.3, 5.0)).is_none());
    }

    #[test]
    fn a_quiet_song_keeps_its_ending() {
        // Everything sits at the absolute floor, so the relative gate is what
        // decides, and nothing is 30 dB below the median.
        let levels = vec![0.005; 120];
        assert!(quiet_cut(&levels).is_none());
    }

    #[test]
    fn a_quiet_ending_in_a_quiet_song_is_still_a_tail() {
        let mut levels = vec![0.02; 40];
        levels.extend(std::iter::repeat_n(0.00005, 80));
        assert!(quiet_cut(&levels).is_some());
    }

    #[test]
    fn no_levels_means_no_verdict() {
        assert!(quiet_cut(&[]).is_none());
        assert!(quiet_cut(&[0.0]).is_none());
    }
}
