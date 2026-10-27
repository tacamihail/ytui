use serde::{Deserialize, Serialize};

/// A single YouTube item: a track, or a playlist entry when browsing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub channel: String,
    #[serde(default)]
    pub duration: u64,
    /// Non-empty when the entry is a raw URL (pasted link, playlist, feed item).
    #[serde(default)]
    pub url_override: String,
}

impl Entry {
    pub fn url(&self) -> String {
        if !self.url_override.is_empty() {
            self.url_override.clone()
        } else {
            format!("https://www.youtube.com/watch?v={}", self.id)
        }
    }

    pub fn duration_str(&self) -> String {
        fmt_duration(self.duration)
    }
}

pub fn fmt_duration(secs: u64) -> String {
    if secs == 0 {
        return "--:--".into();
    }
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn fmt_f64(secs: f64) -> String {
    if !secs.is_finite() || secs <= 0.0 {
        "--:--".into()
    } else {
        fmt_duration(secs as u64)
    }
}
