//! `ytui-resolver update`: reinstall the resolver from github.
//!
//! The binary is installed into the same `--root` it lives in now, so an
//! existing systemd unit, LaunchAgent or Startup entry keeps pointing at a
//! valid executable. Unlike `ytui update` there is no checkout to fast-forward:
//! cargo fetches the repository, builds only the resolver crates and replaces
//! the binary in place.

use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(unix)]
use std::time::Duration;

use crate::service;

const REPO: &str = "https://github.com/tacamihail/ytui";
const COMMITS_URL: &str = "https://api.github.com/repos/tacamihail/ytui/commits/main";
const MANIFEST_PATH: &str = "crates/resolver/Cargo.toml";

const USAGE: &str = "\
ytui-resolver update - reinstall from github

usage:
  ytui-resolver update            install when github has a newer version
  ytui-resolver update --check    only report whether a newer version exists
  ytui-resolver update --force    reinstall even when the version is unchanged

the binary is installed into the same prefix it runs from, so an autostart
entry created with --install-service keeps working. the version is the
[package] version in crates/resolver/Cargo.toml on main.";

pub fn cli(args: &[String]) -> Result<(), String> {
    let mut check = false;
    let mut force = false;
    for arg in args {
        match arg.as_str() {
            "-n" | "--check" => check = true,
            "-f" | "--force" => force = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown option {other}\n\n{USAGE}")),
        }
    }

    let current = env!("CARGO_PKG_VERSION");
    let remote = remote_version()?;
    let newer = is_newer(&remote, current);

    if check {
        if newer {
            println!("ytui-resolver {remote} is available (running {current})");
        } else {
            println!("ytui-resolver {current} is up to date");
        }
        return Ok(());
    }
    if !newer && !force {
        println!("ytui-resolver {current} is up to date (--force reinstalls)");
        return Ok(());
    }

    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot find this binary: {e}\n  {USAGE}"))?;
    let root = install_root(&exe)?;
    println!("· installing ytui-resolver {remote} into {}", root.display());
    install(&root, &remote)?;
    if let Err(e) = service::restart_if_installed() {
        eprintln!("warning: could not restart the autostart entry: {e}");
    }
    Ok(())
}

/// Runs `cargo install` into `root` and reports the new version.
///
/// Shared by the `update` command and the automatic updater, so both fetch the
/// same commit and leave the binary where the autostart entry expects it.
fn install(root: &Path, remote: &str) -> Result<(), String> {
    let status = cargo_command()
        .arg("install")
        .arg("--git")
        .arg(REPO)
        .arg("ytui-resolver")
        .arg("--root")
        .arg(root)
        .arg("--force")
        .status()
        .map_err(|e| {
            format!(
                "cannot run cargo: {e}\n  install by hand with: cargo install --git {REPO} ytui-resolver --force"
            )
        })?;
    if !status.success() {
        return Err("cargo install failed; nothing was changed".into());
    }
    println!("✔ updated to {remote}");
    Ok(())
}

/// Cargo is usually in `~/.cargo/bin`, which a systemd user unit does not have
/// on `PATH`. Prefer the rustup location when it exists.
fn cargo_command() -> Command {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let cargo = PathBuf::from(home)
            .join(".cargo")
            .join("bin")
            .join(format!("cargo{}", std::env::consts::EXE_SUFFIX));
        if cargo.is_file() {
            return Command::new(cargo);
        }
    }
    Command::new("cargo")
}

// ---------------- automatic updates ----------------

/// How often the running server looks for a new release.
#[cfg(unix)]
const AUTO_UPDATE_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
/// Wait for the network and the rest of the boot before the first check.
#[cfg(unix)]
const AUTO_UPDATE_FIRST: Duration = Duration::from_secs(2 * 60);

/// Starts the background updater. Called by the server when auto-update is on
/// (the default).
pub fn spawn_auto() {
    #[cfg(unix)]
    {
        let exe = std::env::current_exe()
            .unwrap_or_else(|_| PathBuf::from(format!("ytui-resolver{}", std::env::consts::EXE_SUFFIX)));
        // A checkout build or a binary outside a `bin` directory cannot be
        // replaced in place; say so once instead of failing every four hours.
        if let Err(e) = install_root(&exe) {
            eprintln!("· auto-update is off: {e}");
            return;
        }
        eprintln!("· auto-update: checking github every 4 hours (--no-auto-update turns this off)");
        std::thread::spawn(move || auto_loop(exe));
    }
    #[cfg(not(unix))]
    {
        eprintln!("· auto-update needs Linux or macOS; run `ytui-resolver update` by hand");
    }
}

#[cfg(unix)]
fn auto_loop(exe: PathBuf) {
    std::thread::sleep(AUTO_UPDATE_FIRST);
    loop {
        match auto_step() {
            Ok(true) => restart_in_place(&exe),
            Ok(false) => {}
            Err(e) => eprintln!("· auto-update: {e}"),
        }
        std::thread::sleep(AUTO_UPDATE_INTERVAL);
    }
}

/// Checks github and installs a newer release. `Ok(true)` means a new binary is
/// on disk and the process should re-exec into it.
#[cfg(unix)]
fn auto_step() -> Result<bool, String> {
    let current = env!("CARGO_PKG_VERSION");
    let remote = remote_version()?;
    if !is_newer(&remote, current) {
        return Ok(false);
    }
    println!("· auto-update: ytui-resolver {remote} is available (running {current})");
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot find this binary: {e}"))?;
    let root = install_root(&exe)?;
    install(&root, &remote)?;
    Ok(true)
}

/// Replaces the process image with the binary that was just installed.
///
/// `exe` is captured before the install on purpose: once cargo has unlinked
/// the old file, `/proc/self/exe` can resolve to `… (deleted)`, which cannot
/// be executed. Re-exec keeps the systemd/launchd main PID, so no service
/// manager restart is needed and the update cannot kill its own build.
#[cfg(unix)]
fn restart_in_place(exe: &Path) -> ! {
    use std::os::unix::process::CommandExt;
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    eprintln!("· auto-update: restarting into the new binary");
    let err = Command::new(exe).args(&args).exec();
    eprintln!("· auto-update: cannot restart ({err}); exiting so the service manager starts it");
    std::process::exit(1);
}

/// The cargo `--root` that owns the running binary. `~/.cargo/bin` and
/// `~/.local/bin` are the two layouts the installers produce.
fn install_root(exe: &Path) -> Result<PathBuf, String> {
    let bin = exe
        .parent()
        .ok_or_else(|| format!("cannot find the directory of {}", exe.display()))?;
    if bin.file_name().and_then(|n| n.to_str()) != Some("bin") {
        return Err(format!(
            "{} is not inside a bin directory, so update cannot replace it in place\n  \
             reinstall with: cargo install --git {REPO} ytui-resolver --force",
            exe.display()
        ));
    }
    bin.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("cannot find the install root of {}", exe.display()))
}

/// Reads the resolver version from github. Uses curl so the server binary does
/// not carry an HTTP/TLS stack just for update checks. The commit sha comes
/// from the API first: raw URLs for a branch are cached for minutes after a
/// push, but a raw URL pinned to a sha is immutable.
fn remote_version() -> Result<String, String> {
    let sha = head_sha()?;
    let url = format!("https://raw.githubusercontent.com/tacamihail/ytui/{sha}/{MANIFEST_PATH}");
    let text = curl(&url)?;
    parse_version(&text).ok_or_else(|| "cannot parse the version from github".into())
}

fn head_sha() -> Result<String, String> {
    let body = curl(COMMITS_URL)?;
    let value: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("bad github response: {e}"))?;
    value
        .get("sha")
        .and_then(|s| s.as_str())
        .map(str::to_string)
        .ok_or_else(|| "github did not report a commit for main".into())
}

fn curl(url: &str) -> Result<String, String> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "15"])
        .arg(url)
        .output()
        .map_err(|e| format!("cannot run curl: {e}\n  check for updates with: cargo install --git {REPO} ytui-resolver --force"))?;
    if !out.status.success() {
        return Err("cannot read the version from github (no network?)".into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// First `version = "x.y.z"` line of a Cargo.toml: the package one comes
/// before any dependency tables.
fn parse_version(toml: &str) -> Option<String> {
    for line in toml.lines() {
        let line = line.trim();
        let Some(rest) = line.strip_prefix("version") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let value = value.trim().trim_matches(['"', '\'']);
        if !value.is_empty() {
            return Some(value.to_string());
        }
    }
    None
}

fn is_newer(remote: &str, current: &str) -> bool {
    match (version_tuple(remote), version_tuple(current)) {
        (Some(r), Some(c)) => r > c,
        _ => remote != current,
    }
}

fn version_tuple(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_package_version_first() {
        let toml = r#"
[package]
name = "ytui-resolver"
version = "0.4.2"
edition = "2021"
rust-version = "1.83"

[dependencies]
serde = { version = "1", features = ["derive"] }
"#;
        assert_eq!(parse_version(toml).as_deref(), Some("0.4.2"));
    }

    #[test]
    fn compares_versions_numerically() {
        assert!(is_newer("0.4.10", "0.4.9"));
        assert!(!is_newer("0.4.1", "0.4.1"));
        assert!(!is_newer("0.4.0", "0.4.1"));
        assert!(is_newer("0.5.0", "0.4.99"));
        assert!(!is_newer("0.4.1-rc1", "0.4.1"));
    }

    #[test]
    fn finds_the_install_root_of_a_bin_directory() {
        let root = install_root(Path::new("/home/me/.cargo/bin/ytui-resolver")).unwrap();
        assert_eq!(root, Path::new("/home/me/.cargo"));
        let root = install_root(Path::new("/home/me/.local/bin/ytui-resolver")).unwrap();
        assert_eq!(root, Path::new("/home/me/.local"));
    }

    #[test]
    fn refuses_a_binary_outside_bin() {
        assert!(install_root(Path::new("/opt/ytui-resolver")).is_err());
    }
}
