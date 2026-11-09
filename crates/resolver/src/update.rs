//! `ytui-resolver update`: reinstall the resolver from github.
//!
//! The binary is installed into the same `--root` it lives in now, so an
//! existing systemd unit, LaunchAgent or Startup entry keeps pointing at a
//! valid executable. Unlike `ytui update` there is no checkout to fast-forward:
//! cargo fetches the repository, builds only the resolver crates and replaces
//! the binary in place.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::service;

const REPO: &str = "https://github.com/tacamihail/ytui";
const VERSION_URL: &str =
    "https://raw.githubusercontent.com/tacamihail/ytui/main/crates/resolver/Cargo.toml";

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
    let status = Command::new("cargo")
        .arg("install")
        .arg("--git")
        .arg(REPO)
        .arg("ytui-resolver")
        .arg("--root")
        .arg(&root)
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
    if let Err(e) = service::restart_if_installed() {
        eprintln!("warning: could not restart the autostart entry: {e}");
    }
    Ok(())
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
/// not carry an HTTP/TLS stack just for update checks.
fn remote_version() -> Result<String, String> {
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "15", VERSION_URL])
        .output()
        .map_err(|e| format!("cannot run curl: {e}\n  check for updates with: cargo install --git {REPO} ytui-resolver --force"))?;
    if !out.status.success() {
        return Err("cannot read the version from github (no network?)".into());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    parse_version(&text).ok_or_else(|| "cannot parse the version from github".into())
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
