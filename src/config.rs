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
    /// Optional `ytui-resolver` base URL, e.g. `http://100.64.0.5:8787`.
    /// Empty means resolve locally with yt-dlp.
    pub resolver_url: String,
    /// Bearer token for the resolver (takes precedence over the file).
    pub resolver_token: String,
    /// File containing the resolver token. Empty = `~/.config/ytui/resolver.token`
    /// when it exists.
    pub resolver_token_file: String,
    /// How long to wait for `/v1/resolve`, seconds.
    pub resolver_timeout_secs: u64,
    /// Fall back to local yt-dlp when the resolver is unreachable or fails.
    pub resolver_fallback: bool,
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
            resolver_url: String::new(),
            resolver_token: String::new(),
            resolver_token_file: String::new(),
            resolver_timeout_secs: 20,
            resolver_fallback: true,
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

/// The user's home directory.
///
/// Unix shells set `HOME`. Windows sets `USERPROFILE`, and the `HOME` that a
/// MSYS/Git Bash environment exports is a POSIX path (`/c/Users/...`) that a
/// native binary cannot use, so it must not win there.
pub(crate) fn home() -> PathBuf {
    #[cfg(windows)]
    let vars: &[&str] = &["USERPROFILE", "HOME"];
    #[cfg(not(windows))]
    let vars: &[&str] = &["HOME"];

    for var in vars {
        if let Some(home) = std::env::var_os(var) {
            if !home.is_empty() {
                return PathBuf::from(home);
            }
        }
    }
    std::env::temp_dir()
}

pub fn config_dir() -> PathBuf {
    home().join(".config/ytui")
}

/// Expand a leading `~`, `~/` or `~\` to the home directory.
///
/// cmd.exe and PowerShell pass `~/cookies.txt` through to a native program
/// unchanged, so the commands in the README have to do this themselves.
pub fn expand_tilde(path: &str) -> PathBuf {
    let rest = path
        .strip_prefix("~/")
        .or_else(|| path.strip_prefix("~\\"))
        .or_else(|| (path == "~").then_some(""));
    match rest {
        Some(rest) => home().join(rest),
        None => PathBuf::from(path),
    }
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
    adopt_cookies_file(&mut cfg, cookies_path().exists());
    if !config_path().exists() {
        save(&cfg);
    }
    cfg
}

/// Picks up `ytui auth <file>` output without needing the user to edit JSON.
///
/// A configured browser profile always wins: `ytui auth --browser` clears
/// `cookies_file` on purpose, and resurrecting a leftover export here would
/// silently switch back to the stale file, which is exactly the "my cookies
/// expired again" trap. Only an empty config adopts `cookies.txt`.
fn adopt_cookies_file(cfg: &mut Config, cookies_exist: bool) {
    if cfg.cookies_file.is_empty() && cfg.cookies_browser.is_empty() && cookies_exist {
        cfg.cookies_file = cookies_path().display().to_string();
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_browser_profile_is_not_overridden_by_a_leftover_export() {
        let mut cfg = Config {
            cookies_browser: "chrome".into(),
            ..Config::default()
        };
        adopt_cookies_file(&mut cfg, true);
        assert!(cfg.cookies_file.is_empty());
        assert_eq!(cfg.cookie_args(), Some(("--cookies-from-browser", "chrome".into())));
    }

    #[test]
    fn an_empty_config_adopts_the_saved_export() {
        let mut cfg = Config::default();
        adopt_cookies_file(&mut cfg, true);
        assert_eq!(cfg.cookies_file, cookies_path().display().to_string());
    }

    #[test]
    fn nothing_is_adopted_without_a_file() {
        let mut cfg = Config::default();
        adopt_cookies_file(&mut cfg, false);
        assert!(cfg.cookies_file.is_empty());
        assert_eq!(cfg.cookie_args(), None);
    }

    #[test]
    fn the_config_dir_hangs_off_the_home_directory() {
        // Regression test: a missing HOME used to fall back to `/tmp`, which on
        // Windows is a path relative to the current drive instead of the user's
        // profile.
        let dir = config_dir();
        assert!(dir.is_absolute(), "{}", dir.display());
        assert!(dir.ends_with(".config/ytui"), "{}", dir.display());
    }

    #[test]
    fn a_tilde_path_expands_to_the_home_directory() {
        assert_eq!(
            expand_tilde("~/x/cookies.txt"),
            home().join("x/cookies.txt")
        );
        assert_eq!(
            expand_tilde("~\\x\\cookies.txt"),
            home().join("x\\cookies.txt")
        );
        assert_eq!(expand_tilde("~"), home());
        assert_eq!(
            expand_tilde("rel/cookies.txt"),
            PathBuf::from("rel/cookies.txt")
        );
        assert_eq!(
            expand_tilde("/abs/cookies.txt"),
            PathBuf::from("/abs/cookies.txt")
        );
    }

    #[test]
    fn an_old_config_without_resolver_fields_still_loads() {
        let cfg: Config = serde_json::from_str(r#"{"results_count": 5, "volume": 10}"#).unwrap();
        assert_eq!(cfg.results_count, 5);
        assert_eq!(cfg.volume, 10);
        assert!(cfg.resolver_url.is_empty());
        assert!(cfg.resolver_token.is_empty());
        assert!(cfg.resolver_token_file.is_empty());
        assert_eq!(cfg.resolver_timeout_secs, 20);
        assert!(cfg.resolver_fallback);
    }
}
