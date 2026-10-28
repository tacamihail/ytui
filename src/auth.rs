//! Connect the player to a YouTube account with cookies.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{self, Config};
use crate::ytdl;

const USAGE: &str = "\
ytui auth - connect your YouTube account

usage:
  ytui auth <cookies.txt>       import a Netscape cookies.txt export
  ytui auth --browser <name>    read cookies from a browser on this machine
  ytui auth --status            show the current connection
  ytui logout                   forget cookies and account

getting cookies.txt (works from Windows into WSL):
  1. install the \"Get cookies.txt LOCALLY\" browser extension
  2. open https://music.youtube.com while signed in
  3. export youtube.com cookies to cookies.txt
  4. ytui auth /mnt/c/Users/<you>/cookies.txt

supported browsers: firefox, chromium, chrome, brave, edge, vivaldi, opera
";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    #[serde(default)]
    pub channel_id: String,
}

pub fn cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        None | Some("-h") | Some("--help") => {
            println!("{USAGE}");
            0
        }
        Some("--status") => status(),
        Some("--browser") => match args.get(1) {
            Some(name) => browser(name),
            None => {
                eprintln!("usage: ytui auth --browser <firefox|chromium|chrome|brave|edge|...>");
                2
            }
        },
        Some(path) => import(path),
    }
}

fn import(path: &str) -> i32 {
    let p = Path::new(path);
    let txt = match fs::read_to_string(p) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("✗ cannot read {path}: {e}");
            return 1;
        }
    };
    if !txt.contains("youtube.com") {
        eprintln!("✗ {path} does not look like a YouTube cookies.txt export");
        return 1;
    }
    if !looks_signed_in(&txt) {
        eprintln!("✗ {path} has no signed-in YouTube cookies");
        eprintln!("  it is missing the first-party session cookie (SID or __Secure-1PSID),");
        eprintln!("  so the export came from a signed-out browser or lost its HttpOnly");
        eprintln!("  cookies. Open https://music.youtube.com in a signed-in browser and");
        eprintln!("  export again with an extension that includes HttpOnly cookies");
        eprintln!("  (\"Get cookies.txt LOCALLY\" does). The existing file was left alone.");
        return 1;
    }
    let dst = config::cookies_path();
    if let Some(dir) = dst.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if let Err(e) = fs::write(&dst, &txt) {
        eprintln!("✗ cannot write {}: {e}", dst.display());
        return 1;
    }
    restrict_permissions(&dst);

    let mut cfg = config::load();
    cfg.cookies_file = dst.display().to_string();
    cfg.cookies_browser.clear();
    config::save(&cfg);

    println!("cookies imported to {}", dst.display());
    verify(&cfg)
}

fn browser(name: &str) -> i32 {
    let mut cfg = config::load();
    cfg.cookies_browser = name.to_string();
    cfg.cookies_file.clear();
    config::save(&cfg);
    println!("using cookies from browser: {name}");
    verify(&cfg)
}

fn verify(cfg: &Config) -> i32 {
    println!("checking account…");
    match ytdl::account(cfg) {
        Ok((name, id)) => {
            let account = Account {
                name: name.clone(),
                channel_id: id.clone(),
            };
            config::save_json(&config::account_path(), &account);
            println!("✔ connected as {name}");
            if !id.is_empty() {
                println!("  channel: https://www.youtube.com/channel/{id}");
            }
            println!("  liked music, playlists and subscriptions are now available");
            0
        }
        Err(e) => {
            eprintln!("✗ could not verify the account: {e}");
            if cfg.cookies_file.is_empty() {
                eprintln!("  the cookies are read from the browser on every run, so nothing is");
                eprintln!("  stored yet. Check that the browser is signed in to youtube.com");
                eprintln!("  and then run `ytui auth <cookies.txt>` with a file export instead");
            } else {
                eprintln!("  cookies were saved to {}", cfg.cookies_file);
                eprintln!("  search and playback already work; Liked Music, Playlists and");
                eprintln!("  Subscriptions need a fresh export from a signed-in session");
                eprintln!("  (open https://music.youtube.com, export youtube.com cookies, retry)");
            }
            eprintln!("  ytui only needs cookies for private lists; it never asks for a password");
            1
        }
    }
}

pub fn status() -> i32 {
    let cfg = config::load();
    match &cfg.cookie_args() {
        Some((flag, value)) => {
            if *flag == "--cookies" {
                println!("cookies: {} ({})", value, flag);
            } else {
                println!("cookies: from browser {value}");
            }
        }
        None => {
            println!("cookies: none (run `ytui auth <cookies.txt>`)");
            return 1;
        }
    }

    // `account.json` only records the last successful verification; the
    // cookies themselves may have been replaced or expired since. Ask YouTube
    // so `--status` means "connected right now", not "connected once".
    match ytdl::account(&cfg) {
        Ok((name, id)) => {
            let account = Account {
                name: name.clone(),
                channel_id: id,
            };
            config::save_json(&config::account_path(), &account);
            println!("✔ account: {name}");
            0
        }
        Err(e) => {
            println!("✗ cookies could not be verified: {e}");
            println!("  re-export cookies.txt from a signed-in browser, then run `ytui auth <file>`");
            1
        }
    }
}

pub fn logout() -> i32 {
    let mut cfg = config::load();
    cfg.cookies_file.clear();
    cfg.cookies_browser.clear();
    config::save(&cfg);
    let _ = fs::remove_file(config::cookies_path());
    let _ = fs::remove_file(config::account_path());
    println!("✔ logged out");
    0
}

pub fn account() -> Option<Account> {
    config::load_json::<Account>(&config::account_path())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

/// Signed-in YouTube cookies always include a first-party session cookie.
/// Exports with only the `__Secure-3P*` variants come from a signed-out
/// browser, or from a tool that skipped the HttpOnly half of the jar.
fn looks_signed_in(txt: &str) -> bool {
    txt.lines().any(|line| {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') {
            return false;
        }
        matches!(
            line.split('\t').nth(5),
            Some("SID" | "__Secure-1PSID" | "SAPISID" | "APISID" | "__Secure-1PAPISID")
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIGNED_IN: &str = "#HttpOnly_.youtube.com\tTRUE\t/\tTRUE\t1999999999\t__Secure-1PSID\tx\n";
    const SIGNED_OUT: &str = ".youtube.com\tTRUE\t/\tTRUE\t1999999999\t__Secure-3PSID\tx\n";

    #[test]
    fn incomplete_exports_are_not_imported() {
        assert!(looks_signed_in(SIGNED_IN));
        assert!(!looks_signed_in(SIGNED_OUT));
        assert!(!looks_signed_in("# comment\n"));
    }
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}
