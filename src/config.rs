//! Config + on-disk locations.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Netscape cookies.txt exported from a logged-in browser.
    pub cookies_file: String,
    /// Alternative: let yt-dlp read a browser profile directly (same machine only).
    pub cookies_browser: String,
    /// Search results per query.
    pub results_count: u32,
    /// Max entries pulled from a playlist / feed.
    pub fetch_limit: u32,
    /// History entries kept.
    pub history_limit: usize,
    /// Arrow / h l seek step, seconds.
    pub seek_step: f64,
    /// [ ] volume step, percent.
    pub volume_step: i64,
    /// Volume at startup, percent. Playback starts quiet on purpose.
    pub volume: i64,
    /// Audio ring buffer in ms. Lower = less RAM, more underrun risk.
    pub buffer_ms: u32,
    /// HTTP range cache chunk in KiB (one chunk is resident at a time).
    pub cache_kib: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            cookies_file: String::new(),
            cookies_browser: String::new(),
            results_count: 30,
            fetch_limit: 2000,
            history_limit: 300,
            seek_step: 5.0,
            volume_step: 5,
            volume: 35,
            buffer_ms: 2000,
            cache_kib: 256,
        }
    }
}

impl Config {
    /// `--cookies <file>` or `--cookies-from-browser <name>`, if configured.
    pub fn cookie_args(&self) -> Option<(&'static str, String)> {
        if !self.cookies_file.is_empty() && Path::new(&self.cookies_file).exists() {
            Some(("--cookies", self.cookies_file.clone()))
        } else if !self.cookies_browser.is_empty() {
            Some(("--cookies-from-browser", self.cookies_browser.clone()))
        } else {
            None
        }
    }

    pub fn has_cookies(&self) -> bool {
        self.cookie_args().is_some()
    }

    pub fn cookie_path(&self) -> Option<PathBuf> {
        match &self.cookie_args() {
            Some(("--cookies", p)) => Some(PathBuf::from(p)),
            _ => None,
        }
    }
}

pub(crate) fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

pub fn config_dir() -> PathBuf {
    home().join(".config/ytui")
}

pub fn data_dir() -> PathBuf {
    home().join(".local/share/ytui")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.json")
}

pub fn cookies_path() -> PathBuf {
    config_dir().join("cookies.txt")
}

pub fn account_path() -> PathBuf {
    config_dir().join("account.json")
}

/// Checkout that `install.sh` built from; `ytui update` pulls and rebuilds it.
pub fn source_path() -> PathBuf {
    config_dir().join("source")
}

pub fn history_path() -> PathBuf {
    data_dir().join("history.json")
}

pub fn load() -> Config {
    let mut cfg = match fs::read_to_string(config_path()) {
        Ok(txt) => serde_json::from_str::<Config>(&txt).unwrap_or_default(),
        Err(_) => Config::default(),
    };
    // Pick up `ytui auth` output without needing the user to edit JSON.
    if cfg.cookies_file.is_empty() && cookies_path().exists() {
        cfg.cookies_file = cookies_path().display().to_string();
    }
    if !config_path().exists() {
        save(&cfg);
    }
    cfg
}

pub fn save(cfg: &Config) {
    let _ = fs::create_dir_all(config_dir());
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        let _ = fs::write(config_path(), s);
    }
}

pub fn load_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let txt = fs::read_to_string(path).ok()?;
    serde_json::from_str(&txt).ok()
}

pub fn save_json<T: Serialize>(path: &Path, value: &T) {
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Ok(s) = serde_json::to_string_pretty(value) {
        let _ = fs::write(path, s);
    }
}
