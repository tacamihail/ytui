//! `ytui update`: install the newer version from github.
//!
//! There are two kinds of install. A checkout the installer built is updated
//! with a `git pull` plus a release build; whether one is needed is decided by
//! the `[package] version` in github's `Cargo.toml`, not by how many commits the
//! checkout is behind: the version is bumped by hand when a release is cut, so
//! an unbumped commit is not something to install. `install.sh` (or
//! `install.ps1` on Windows) records the checkout it built from in
//! `~/.config/ytui/source`; `$YTUI_SRC` overrides it. Only fast-forwards are
//! done: a checkout with local commits, uncommitted changes or a diverged
//! history is left alone so nothing of the user's work is touched.
//!
//! A release install is the binary the installers downloaded from a github
//! release asset. It has no checkout, so an update downloads the asset from the
//! latest release and replaces the installed binary in place.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::config;

const BRANCH: &str = "main";
const REPO: &str = "tacamihail/ytui";
/// github asks for a product token on the API and on downloads.
const UA: &str = "ytui-updater";

/// The installer that records the checkout and refreshes the tools.
#[cfg(windows)]
const INSTALLER: &str = r".\install.ps1";
#[cfg(not(windows))]
const INSTALLER: &str = "./install.sh";

const USAGE: &str = "\
ytui update - install the newer version from github

usage:
  ytui update            install when github has a newer version
  ytui update --check    only report whether a newer version exists
  ytui update --force    rebuild even when the version is unchanged

the version is the [package] version in Cargo.toml; it is bumped by hand when
a release is cut, so commits that do not change it are not installed. a
checkout is found through $YTUI_SRC, else the path the installer saved in
~/.config/ytui/source, else the current directory if it looks like the repo.
without a checkout the install is treated as a release download and the asset
is fetched from github's latest release.
";

/// Where the running binary came from, which decides how it is updated.
enum Install {
    /// A checkout the installer built; `ytui update` pulls and rebuilds it.
    Source(PathBuf),
    /// A release asset the installer downloaded.
    Release,
}

pub fn cli(args: &[String]) -> i32 {
    match run(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("✗ {e}");
            1
        }
    }
}

/// The version on github when it is newer than the running binary; `None` when
/// this build is already current. Fetches first, so call it from a worker
/// thread: it needs the network. `Err` when there is no checkout to compare,
/// the version cannot be read or the network fails.
pub fn available() -> Result<Option<String>, String> {
    match install() {
        Install::Source(dir) => {
            let dir = checkout_at(dir)?;
            git(&dir, &["fetch", "--quiet", "origin", BRANCH])?;
            let remote = version_at_ref(&dir, &format!("origin/{BRANCH}"))?;
            Ok(is_newer(&remote, version()).then_some(remote))
        }
        Install::Release => {
            let remote = latest_release()?;
            Ok(is_newer(&remote, version()).then_some(remote))
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

    match install() {
        Install::Source(dir) => run_source(&checkout_at(dir)?, check, force),
        Install::Release => run_release(check, force),
    }
}

/// Pull, rebuild and install from the recorded checkout.
fn run_source(dir: &Path, check: bool, force: bool) -> Result<(), String> {
    println!("· checkout {}", dir.display());

    let remote = format!("origin/{BRANCH}");
    let local = git(dir, &["rev-parse", "HEAD"])?;
    println!("· fetching github.com/{REPO} ...");
    git(dir, &["fetch", "--quiet", "origin", BRANCH])
        .map_err(|e| format!("{e}\n  check the network and try again"))?;
    let target = git(dir, &["rev-parse", &remote])?;
    let remote_version = version_at_ref(dir, &remote)?;
    let newer = is_newer(&remote_version, version());

    if check {
        if newer {
            println!(
                "· ytui {remote_version} is available (running {}), run `ytui update` to install",
                version()
            );
        } else {
            println!(
                "✔ already up to date ({} · ytui {})",
                short(&local),
                version()
            );
        }
        return Ok(());
    }

    if !newer && !force {
        println!(
            "✔ already up to date ({} · ytui {})",
            short(&local),
            version()
        );
        return Ok(());
    }

    if local == target {
        // The checkout already carries the new version, or --force asked for a
        // rebuild: only the binary has to be replaced.
        return build_and_install(dir, &local, None);
    }

    let behind = is_ancestor(dir, "HEAD", &remote)?;
    if !behind {
        if is_ancestor(dir, &remote, "HEAD")? {
            println!("· ahead of github, nothing to pull");
            return if force {
                build_and_install(dir, &local, None)
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

    let branch = git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch != BRANCH {
        return Err(format!(
            "the checkout is on `{branch}`, not `{BRANCH}`\n  \
             switch to {BRANCH} first: git -C {} switch {BRANCH}",
            dir.display()
        ));
    }

    println!(
        "· ytui {remote_version} is available (running {})",
        version()
    );

    if !git(dir, &["status", "--porcelain"])?.is_empty() {
        return Err(format!(
            "{} has uncommitted changes\n  commit or stash them first, \
             then run ytui update again",
            dir.display()
        ));
    }

    println!("· fast-forwarding {} -> {}", short(&local), short(&target));
    git(dir, &["merge", "--ff-only", &remote])
        .map_err(|e| format!("{e}\n  the checkout was not changed beyond the fetch"))?;

    let touched_installer = git(dir, &["diff", "--name-only", &local, &target])?
        .lines()
        .map(str::trim)
        .any(|l| l == "install.sh" || l == "install.ps1");

    build_and_install(dir, &target, Some(touched_installer))
}

/// Download and install the asset from the latest release.
fn run_release(check: bool, force: bool) -> Result<(), String> {
    let asset = release_asset()?;
    println!("· asking github.com/{REPO} for the latest release ...");
    let remote = latest_release()?;
    let newer = is_newer(&remote, version());

    if check {
        if newer {
            println!(
                "· ytui {remote} is available (running {}), run `ytui update` to install",
                version()
            );
        } else {
            println!("✔ already up to date (ytui {})", version());
        }
        return Ok(());
    }

    if !newer && !force {
        println!("✔ already up to date (ytui {})", version());
        return Ok(());
    }

    println!("· ytui {remote} is available (running {})", version());
    let downloaded = download_release(asset)?;
    let installed = place_binary(&downloaded);
    let _ = fs::remove_file(&downloaded);
    let installed = installed?;

    println!("✔ updated to ytui {remote}");
    println!("  installed -> {}", installed.display());
    println!("  restart ytui to use the new binary");
    Ok(())
}

/// The asset name `release.yml` publishes for this target.
fn release_asset() -> Result<&'static str, String> {
    let asset = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("windows", "x86_64") => "ytui-x86_64-pc-windows-msvc.exe",
        ("windows", "aarch64") => "ytui-aarch64-pc-windows-msvc.exe",
        ("linux", "x86_64") => "ytui-x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "ytui-aarch64-unknown-linux-gnu",
        ("macos", "x86_64") => "ytui-x86_64-apple-darwin",
        ("macos", "aarch64") => "ytui-aarch64-apple-darwin",
        (os, arch) => {
            return Err(format!(
                "no release build for {os}/{arch}\n  \
                 rerun {INSTALLER} to build from source"
            ))
        }
    };
    Ok(asset)
}

/// The `tag_name` of github's latest release, without the leading `v`.
fn latest_release() -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let body = ureq::get(&url)
        .set("User-Agent", UA)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("github did not answer\n  {e}"))?
        .into_string()
        .map_err(|e| format!("cannot read github's answer: {e}"))?;
    let json: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("cannot parse github's answer: {e}"))?;
    let tag = json
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or("the latest release has no tag name")?;
    Ok(tag.trim_start_matches('v').to_string())
}

/// Fetch the latest release asset into a temporary file.
fn download_release(asset: &str) -> Result<PathBuf, String> {
    let url = format!("https://github.com/{REPO}/releases/latest/download/{asset}");
    println!("· downloading {asset}");
    let resp = ureq::get(&url)
        .set("User-Agent", UA)
        .call()
        .map_err(|e| format!("cannot download {url}\n  {e}"))?;
    let tmp = std::env::temp_dir().join(format!("ytui-{}-{asset}", std::process::id()));
    let mut out =
        fs::File::create(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
    std::io::copy(&mut resp.into_reader(), &mut out)
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    Ok(tmp)
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

    let built = dir
        .join("target/release")
        .join(format!("ytui{}", std::env::consts::EXE_SUFFIX));
    if !built.is_file() {
        return Err(format!("{} was not produced", built.display()));
    }

    let dst = place_binary(&built)?;
    let version = version_at(dir).unwrap_or_else(|| version().to_string());
    println!("✔ updated to {} · ytui {version}", short(sha));
    println!("  installed -> {}", dst.display());
    if installer_changed == Some(true) {
        println!(
            "  the installer changed too, rerun {INSTALLER} to refresh yt-dlp, deno and the launcher"
        );
    }
    println!("  restart ytui to use the new binary");
    Ok(())
}

/// Copy `src` over the installed binary and refresh the macOS bundle identity.
fn place_binary(src: &Path) -> Result<PathBuf, String> {
    let dst = bin_path();
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    // Copy next to the destination and rename over it: writing a running
    // executable in place fails with ETXTBSY on Linux.
    let tmp = dst.with_extension("new");
    fs::copy(src, &tmp).map_err(|e| format!("cannot install {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755));
    }
    replace_binary(&tmp, &dst).map_err(|e| format!("cannot replace {}: {e}", dst.display()))?;

    #[cfg(target_os = "macos")]
    {
        // The app bundle is what carries the name and icon into Now Playing and
        // the code signature is what gives the process its identity; refresh
        // both after replacing the executable.
        match app_bundle_of(&dst) {
            Some(app) => sign_and_register(app),
            None => println!(
                "  ! the macOS app bundle is missing; rerun ./install.sh to get \
                 the Now Playing panel and icon"
            ),
        }
    }
    Ok(dst)
}

/// Whether this is a checkout the installer recorded, or a release download.
fn install() -> Install {
    // The installers delete `source` when they install a release asset, so its
    // absence means there is no checkout to pull from.
    match source_dir() {
        Some(dir) => Install::Source(dir),
        None => Install::Release,
    }
}

/// Where the installer puts the real binary; `$YTUI_BIN` overrides it.
fn bin_path() -> PathBuf {
    if let Ok(v) = std::env::var("YTUI_BIN") {
        let v = v.trim();
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    platform_bin_path()
}

/// macOS runs the copy inside the app bundle, so Now Playing, Control Center
/// and the Touch Bar can show the ytui name and icon. Installs made before the
/// bundle existed keep their plain binary and get a warning after updating.
#[cfg(target_os = "macos")]
fn platform_bin_path() -> PathBuf {
    let app = app_bundle_path();
    if app.join("Contents/Info.plist").is_file() {
        app.join("Contents/MacOS/ytui")
    } else {
        config::home().join(".local/bin/ytui.bin")
    }
}

#[cfg(not(target_os = "macos"))]
fn platform_bin_path() -> PathBuf {
    // install.ps1 puts `ytui.exe` in `%USERPROFILE%\.local\bin`; install.sh
    // puts `ytui.bin` next to the launcher.
    #[cfg(windows)]
    return config::home().join(format!(".local/bin/ytui{}", std::env::consts::EXE_SUFFIX));
    #[cfg(not(windows))]
    config::home().join(".local/bin/ytui.bin")
}

#[cfg(target_os = "macos")]
fn app_bundle_path() -> PathBuf {
    config::home().join(".local/lib/ytui/ytui.app")
}

/// `.../ytui.app/Contents/MacOS/ytui` -> `.../ytui.app`.
#[cfg(target_os = "macos")]
fn app_bundle_of(exe: &Path) -> Option<&Path> {
    let macos = exe.parent()?;
    if macos.file_name()?.to_str()? != "MacOS" {
        return None;
    }
    let contents = macos.parent()?;
    if contents.file_name()?.to_str()? != "Contents" {
        return None;
    }
    let app = contents.parent()?;
    (app.extension()?.to_str()? == "app").then_some(app)
}

#[cfg(target_os = "macos")]
fn sign_and_register(app: &Path) {
    let signed = Command::new("codesign")
        .args(["-f", "-s", "-", "-i", "dev.tacamihail.ytui"])
        .arg(app)
        .status();
    match signed {
        Ok(status) if status.success() => {}
        _ => println!(
            "  ! could not re-sign {}; Now Playing may show an old name",
            app.display()
        ),
    }
    const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
    let _ = Command::new(LSREGISTER).arg("-f").arg(app).status();
}

/// The recorded source checkout, validated as the ytui repo.
fn checkout_at(dir: PathBuf) -> Result<PathBuf, String> {
    if !dir.join("Cargo.toml").is_file() || !dir.join(".git").exists() {
        return Err(format!(
            "{} is not the ytui checkout\n  expected Cargo.toml and .git there, \
             set YTUI_SRC or rerun {INSTALLER}",
            dir.display()
        ));
    }
    Ok(dir)
}

/// `$YTUI_SRC`, then the path the installer saved, then the current directory
/// when it looks like the repo. `None` means this is a release install.
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

/// Move `tmp` into place.
///
/// On Windows the running `ytui.exe` cannot be overwritten, but it can be
/// renamed, so it is moved out of the way first and the leftover is removed
/// (or replaced by the next update).
fn replace_binary(tmp: &Path, dst: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let old = dst.with_extension("old");
        let _ = fs::remove_file(&old);
        let moved = fs::rename(dst, &old).is_ok();
        if let Err(e) = fs::rename(tmp, dst) {
            if moved {
                let _ = fs::rename(&old, dst);
            }
            return Err(e);
        }
        let _ = fs::remove_file(&old);
        Ok(())
    }
    #[cfg(not(windows))]
    {
        fs::rename(tmp, dst)
    }
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
    parse_package_version(&fs::read_to_string(dir.join("Cargo.toml")).ok()?)
}

/// Version in `rev`'s `Cargo.toml`, read through git so the working tree is
/// never touched. `rev` is `origin/main` here, already fetched.
fn version_at_ref(dir: &Path, rev: &str) -> Result<String, String> {
    let txt = git(dir, &["show", &format!("{rev}:Cargo.toml")])
        .map_err(|e| format!("cannot read Cargo.toml from {rev}\n  {e}"))?;
    parse_package_version(&txt)
        .ok_or_else(|| format!("{rev} has no [package] version in Cargo.toml"))
}

/// The `[package] version`, ignoring dependency versions.
fn parse_package_version(txt: &str) -> Option<String> {
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

/// Compare dotted numeric versions, so `0.3.10` is newer than `0.3.9` rather
/// than sorting as text. An unparsable version only counts as newer when it
/// differs, so a hand-written tag still reaches the user.
fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(a), Some(b)) => a > b,
        _ => candidate.trim() != current.trim(),
    }
}

fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number_not_text() {
        assert!(is_newer("0.3.10", "0.3.9"));
        assert!(is_newer("0.4.0", "0.3.99"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.3.18", "0.3.18"));
        assert!(!is_newer("0.3.17", "0.3.18"));
        assert!(!is_newer("0.3.9", "0.3.10"));
    }

    #[test]
    fn the_package_version_is_read_from_cargo_toml() {
        let toml = "[package]\nname = \"ytui\"\nversion = \"1.2.3\"\n\n\
                    [dependencies.serde]\nversion = \"9.9.9\"\n";
        assert_eq!(parse_package_version(toml).as_deref(), Some("1.2.3"));
        assert_eq!(
            parse_package_version("[dependencies.serde]\nversion = \"9.9.9\"\n"),
            None
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_app_bundle_is_found_from_the_executable_path() {
        let exe = Path::new("/Users/me/.local/lib/ytui/ytui.app/Contents/MacOS/ytui");
        assert_eq!(
            app_bundle_of(exe),
            Some(Path::new("/Users/me/.local/lib/ytui/ytui.app"))
        );
        assert!(app_bundle_of(Path::new("/Users/me/.local/bin/ytui.bin")).is_none());
    }
}
