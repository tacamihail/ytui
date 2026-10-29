//! `ytui update`: fast-forward the source checkout and reinstall the binary.
//!
//! ytui is distributed as source, so an update is a `git pull` plus a release
//! build. `install.sh` records the checkout it built from in
//! `~/.config/ytui/source`; `$YTUI_SRC` overrides it. Only fast-forwards are
//! done: a checkout with local commits, uncommitted changes or a diverged
//! history is left alone so nothing of the user's work is touched.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config;

const BRANCH: &str = "main";

const USAGE: &str = "\
ytui update - pull the latest source and rebuild

usage:
  ytui update            fetch, fast-forward and reinstall
  ytui update --check    only report whether newer commits exist
  ytui update --force    rebuild even when there is nothing to pull

the checkout is found through $YTUI_SRC, else the path install.sh saved in
~/.config/ytui/source, else the current directory if it looks like the repo
";

pub fn cli(args: &[String]) -> i32 {
    match run(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("✗ {e}");
            1
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let mut check = false;
    let mut force = false;
    for a in args {
        match a.as_str() {
            "--check" | "-n" => check = true,
            "--force" | "-f" => force = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => return Err(format!("unknown option: {other}\n\n{USAGE}")),
        }
    }

    let dir = source_dir().ok_or(
        "no source checkout recorded\n  \
         run ./install.sh from the ytui checkout once, or set YTUI_SRC",
    )?;
    if !dir.join("Cargo.toml").is_file() || !dir.join(".git").exists() {
        return Err(format!(
            "{} is not the ytui checkout\n  expected Cargo.toml and .git there, \
             set YTUI_SRC or rerun install.sh",
            dir.display()
        ));
    }
    println!("· checkout {}", dir.display());

    let remote = format!("origin/{BRANCH}");
    let local = git(&dir, &["rev-parse", "HEAD"])?;
    println!("· fetching github.com/tacamihail/ytui ...");
    git(&dir, &["fetch", "--quiet", "origin", BRANCH])
        .map_err(|e| format!("{e}\n  check the network and try again"))?;
    let target = git(&dir, &["rev-parse", &remote])?;

    if local == target && !force {
        println!(
            "✔ already up to date ({} · ytui {})",
            short(&local),
            version()
        );
        return Ok(());
    }

    let rebuild_only = local == target;
    if rebuild_only {
        // --force with nothing to pull: just rebuild what is there.
        return build_and_install(&dir, &local, None);
    }

    let behind = is_ancestor(&dir, "HEAD", &remote)?;
    if !behind {
        if is_ancestor(&dir, &remote, "HEAD")? {
            println!("· ahead of github, nothing to pull");
            return if force {
                build_and_install(&dir, &local, None)
            } else {
                Ok(())
            };
        }
        return Err(format!(
            "the checkout and github have diverged\n  \
             local {}\n  github {}\n  \
             rebase or reset {} by hand, then run ytui update again",
            short(&local),
            short(&target),
            dir.display()
        ));
    }

    let branch = git(&dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch != BRANCH {
        return Err(format!(
            "the checkout is on `{branch}`, not `{BRANCH}`\n  \
             switch to {BRANCH} first: git -C {} switch {BRANCH}",
            dir.display()
        ));
    }

    let log = git(
        &dir,
        &[
            "log",
            "--oneline",
            "--no-decorate",
            &format!("HEAD..{remote}"),
        ],
    )?;
    println!("· newer commits on github:");
    for line in log.lines() {
        println!("    {line}");
    }

    if check {
        println!(
            "· {} is {} behind {remote}, run `ytui update` to install",
            short(&local),
            log.lines().count()
        );
        return Ok(());
    }

    if !git(&dir, &["status", "--porcelain"])?.is_empty() {
        return Err(format!(
            "{} has uncommitted changes\n  commit or stash them first, \
             then run ytui update again",
            dir.display()
        ));
    }

    println!("· fast-forwarding {} -> {}", short(&local), short(&target));
    git(&dir, &["merge", "--ff-only", &remote])
        .map_err(|e| format!("{e}\n  the checkout was not changed beyond the fetch"))?;

    let touched_installer = git(&dir, &["diff", "--name-only", &local, &target])?
        .lines()
        .any(|l| l.trim() == "install.sh");

    build_and_install(&dir, &target, Some(touched_installer))
}

fn build_and_install(dir: &Path, sha: &str, installer_changed: Option<bool>) -> Result<(), String> {
    println!("· building (cargo build --release), this takes a minute ...");
    let status = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .current_dir(dir)
        .status()
        .map_err(|e| format!("cannot run cargo: {e}\n  is Rust installed? https://rustup.rs"))?;
    if !status.success() {
        return Err("build failed, the installed binary was left alone".into());
    }

    let built = dir.join("target/release/ytui");
    if !built.is_file() {
        return Err(format!("{} was not produced", built.display()));
    }

    let dst = bin_path();
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    // Copy next to the destination and rename over it: writing a running
    // executable in place fails with ETXTBSY on Linux.
    let tmp = dst.with_extension("new");
    fs::copy(&built, &tmp).map_err(|e| format!("cannot install {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755));
    }
    fs::rename(&tmp, &dst).map_err(|e| format!("cannot replace {}: {e}", dst.display()))?;

    let version = version_at(dir).unwrap_or_else(|| version().to_string());
    println!("✔ updated to {} · ytui {version}", short(sha));
    println!("  installed -> {}", dst.display());
    if installer_changed == Some(true) {
        println!(
            "  install.sh changed too, rerun ./install.sh to refresh yt-dlp, deno and the launcher"
        );
    }
    println!("  restart ytui to use the new binary");
    Ok(())
}

/// `$YTUI_SRC`, then the path `install.sh` saved, then the current directory.
fn source_dir() -> Option<PathBuf> {
    if let Ok(v) = std::env::var("YTUI_SRC") {
        let v = v.trim();
        if !v.is_empty() {
            return Some(PathBuf::from(v));
        }
    }
    if let Ok(txt) = fs::read_to_string(config::source_path()) {
        let t = txt.trim();
        if !t.is_empty() {
            return Some(PathBuf::from(t));
        }
    }
    let cwd = std::env::current_dir().ok()?;
    if cwd.join("Cargo.toml").is_file() && cwd.join(".git").exists() {
        return Some(cwd);
    }
    None
}

/// Where `install.sh` puts the real binary; `$YTUI_BIN` overrides it.
fn bin_path() -> PathBuf {
    if let Ok(v) = std::env::var("YTUI_BIN") {
        let v = v.trim();
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    config::home().join(".local/bin/ytui.bin")
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git {} failed\n  {}", args.join(" "), err.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `git merge-base --is-ancestor`, whose exit code is the answer.
fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(format!(
            "git merge-base failed\n  {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

/// Version in the checkout that was just built; the running binary may be
/// older than the commit it installed.
fn version_at(dir: &Path) -> Option<String> {
    let txt = fs::read_to_string(dir.join("Cargo.toml")).ok()?;
    let mut package = false;
    for line in txt.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            package = line == "[package]";
            continue;
        }
        if package {
            if let Some(rest) = line.strip_prefix("version") {
                let value = rest.trim_start().strip_prefix('=')?.trim();
                return Some(value.trim_matches('"').to_string());
            }
        }
    }
    None
}

fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
