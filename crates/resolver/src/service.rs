//! Autostart for `ytui-resolver`.
//!
//! One flag installs the native entry for the current OS: a systemd user unit
//! on Linux, a LaunchAgent on macOS, and a Startup shortcut script on Windows.
//! The server binary carries all of it, so a resolver host never needs the TUI,
//! the rest of the project, or administrator rights.

use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Command;

/// launchd label; also the stem of the Windows Startup entry.
#[cfg(any(target_os = "macos", test))]
pub const LABEL: &str = "com.ytui.resolver";
/// systemd unit name.
#[cfg(target_os = "linux")]
pub const UNIT: &str = "ytui-resolver.service";

/// What the service should run: this binary plus the options it was given,
/// without the service flags themselves.
pub struct Spec {
    pub exe: PathBuf,
    pub args: Vec<String>,
}

/// Installs the autostart entry and starts it now where the platform supports
/// that. Rerunning replaces an existing entry.
pub fn install(spec: &Spec) -> Result<(), String> {
    platform::install(spec)
}

/// Stops and removes the autostart entry. Missing entries are not an error.
pub fn uninstall() -> Result<(), String> {
    platform::uninstall()
}

// ---------------- generated files (platform independent) ----------------

#[cfg(any(target_os = "linux", test))]
pub fn systemd_unit(spec: &Spec) -> String {
    let mut exec = systemd_quote(&spec.exe.to_string_lossy());
    for arg in &spec.args {
        exec.push(' ');
        exec.push_str(&systemd_quote(arg));
    }
    format!(
        "\
[Unit]
Description=ytui remote resolver
Documentation=https://github.com/tacamihail/ytui/blob/main/docs/resolver.md
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exec}
Restart=on-failure
RestartSec=3
NoNewPrivileges=true
PrivateTmp=true

[Install]
WantedBy=default.target
"
    )
}

#[cfg(any(target_os = "macos", test))]
pub fn launchd_plist(spec: &Spec, log: &Path) -> String {
    let exe = spec.exe.to_string_lossy().into_owned();
    let mut items = String::new();
    for arg in std::iter::once(&exe).chain(spec.args.iter()) {
        items.push_str("    <string>");
        items.push_str(&xml_escape(arg));
        items.push_str("</string>\n");
    }
    let log = xml_escape(&log.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{items}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#
    )
}

#[cfg(any(windows, test))]
pub fn startup_cmd(spec: &Spec) -> String {
    let mut cmd = String::from("@echo off\r\nstart \"\" /min ");
    cmd.push_str(&cmd_quote(&spec.exe.to_string_lossy()));
    for arg in &spec.args {
        cmd.push(' ');
        cmd.push_str(&cmd_quote(arg));
    }
    cmd.push_str("\r\n");
    cmd
}

/// Quote for a systemd `ExecStart=` line. Plain paths and flags stay readable.
#[cfg(any(target_os = "linux", test))]
fn systemd_quote(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@=+,".contains(c));
    if plain {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// Quote for a `cmd.exe` command line.
#[cfg(any(windows, test))]
fn cmd_quote(value: &str) -> String {
    let plain = !value.is_empty() && !value.contains([' ', '\t', '"']);
    if plain {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('"', "\"\""))
    }
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// ---------------- shared paths and helpers ----------------

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn write_file(path: &Path, body: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    std::fs::write(path, body).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

fn remove_file(path: &Path, what: &str) -> Result<(), String> {
    if !path.exists() {
        println!("{what} was not installed");
        return Ok(());
    }
    std::fs::remove_file(path).map_err(|e| format!("cannot remove {}: {e}", path.display()))?;
    println!("removed {}", path.display());
    Ok(())
}

#[cfg(target_os = "linux")]
fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run {cmd}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        Err(format!("{cmd} failed"))
    } else {
        Err(format!("{cmd} failed: {stderr}"))
    }
}

// ---------------- Linux: systemd user unit ----------------

#[cfg(target_os = "linux")]
mod platform {
    use super::*;

    fn unit_path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home_dir().join(".config"));
        base.join("systemd").join("user").join(UNIT)
    }

    pub fn install(spec: &Spec) -> Result<(), String> {
        let path = unit_path();
        write_file(&path, &systemd_unit(spec))?;
        run("systemctl", &["--user", "daemon-reload"])
            .map_err(|e| format!("{e}\n  unit written to {}, start it by hand if systemd is unavailable", path.display()))?;
        run("systemctl", &["--user", "enable", "--now", UNIT])?;
        println!("installed {} (starts at login)", path.display());
        if let Some(user) = std::env::var_os("USER") {
            println!(
                "to keep it running without a login session: loginctl enable-linger {}",
                user.to_string_lossy()
            );
        }
        Ok(())
    }

    pub fn uninstall() -> Result<(), String> {
        let path = unit_path();
        let _ = run("systemctl", &["--user", "disable", "--now", UNIT]);
        remove_file(&path, "the systemd unit")?;
        let _ = run("systemctl", &["--user", "daemon-reload"]);
        Ok(())
    }
}

// ---------------- macOS: LaunchAgent ----------------

#[cfg(target_os = "macos")]
mod platform {
    use super::*;

    fn agent_path() -> PathBuf {
        home_dir().join("Library").join("LaunchAgents").join(format!("{LABEL}.plist"))
    }

    fn log_path() -> PathBuf {
        home_dir().join(".config").join("ytui").join("resolver.log")
    }

    pub fn install(spec: &Spec) -> Result<(), String> {
        let path = agent_path();
        let log = log_path();
        write_file(&path, &launchd_plist(spec, &log))?;
        let _ = Command::new("launchctl").arg("unload").arg("-w").arg(&path).output();
        let out = Command::new("launchctl")
            .arg("load")
            .arg("-w")
            .arg(&path)
            .output()
            .map_err(|e| format!("cannot run launchctl: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "launchctl failed: {}\n  agent written to {}",
                stderr.trim(),
                path.display()
            ));
        }
        println!("installed {} (starts at login, logs in {})", path.display(), log.display());
        Ok(())
    }

    pub fn uninstall() -> Result<(), String> {
        let path = agent_path();
        let _ = Command::new("launchctl").arg("unload").arg("-w").arg(&path).output();
        remove_file(&path, "the LaunchAgent")
    }
}

// ---------------- Windows: Startup folder script ----------------

#[cfg(windows)]
mod platform {
    use super::*;

    fn startup_path() -> Result<PathBuf, String> {
        let appdata = std::env::var_os("APPDATA")
            .ok_or("APPDATA is not set, cannot find the Startup folder")?;
        Ok(PathBuf::from(appdata)
            .join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join("Startup")
            .join("ytui-resolver.cmd"))
    }

    pub fn install(spec: &Spec) -> Result<(), String> {
        let path = startup_path()?;
        write_file(&path, &startup_cmd(spec))?;
        println!("installed {} (starts at your next login)", path.display());
        println!("start it now with the same command without --install-service");
        Ok(())
    }

    pub fn uninstall() -> Result<(), String> {
        remove_file(&startup_path()?, "the Startup entry")
    }
}

// ---------------- anything else ----------------

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod platform {
    use super::*;

    pub fn install(_spec: &Spec) -> Result<(), String> {
        Err("autostart is only implemented for Linux, macOS and Windows".into())
    }

    pub fn uninstall() -> Result<(), String> {
        Err("autostart is only implemented for Linux, macOS and Windows".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> Spec {
        Spec {
            exe: PathBuf::from("/home/me/bin/ytui-resolver"),
            args: vec![
                "--bind".into(),
                "0.0.0.0:8787".into(),
                "--allow-private".into(),
            ],
        }
    }

    #[test]
    fn systemd_unit_runs_the_same_options() {
        let unit = systemd_unit(&spec());
        assert!(unit.contains(
            "ExecStart=/home/me/bin/ytui-resolver --bind 0.0.0.0:8787 --allow-private\n"
        ));
        assert!(unit.contains("WantedBy=default.target"));
        assert!(unit.contains("Restart=on-failure"));
    }

    #[test]
    fn systemd_quotes_a_path_with_spaces() {
        let unit = systemd_unit(&Spec {
            exe: PathBuf::from("/home/me/My Tools/ytui-resolver"),
            args: vec!["--token-file".into(), "/home/me/My Tokens/t".into()],
        });
        assert!(unit.contains(
            "ExecStart=\"/home/me/My Tools/ytui-resolver\" --token-file \"/home/me/My Tokens/t\"\n"
        ));
    }

    #[test]
    fn launchd_agent_lists_every_argument() {
        let plist = launchd_plist(&spec(), Path::new("/home/me/.config/ytui/resolver.log"));
        assert!(plist.contains("<string>/home/me/bin/ytui-resolver</string>"));
        assert!(plist.contains("<string>--allow-private</string>"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("/home/me/.config/ytui/resolver.log"));
        assert!(plist.contains(&format!("<string>{LABEL}</string>")));
    }

    #[test]
    fn launchd_escapes_xml() {
        let plist = launchd_plist(
            &Spec {
                exe: PathBuf::from("/tmp/a&b/ytui-resolver"),
                args: Vec::new(),
            },
            Path::new("/tmp/x.log"),
        );
        assert!(plist.contains("<string>/tmp/a&amp;b/ytui-resolver</string>"));
        assert!(!plist.contains("a&b"));
    }

    #[test]
    fn startup_entry_starts_minimized() {
        let cmd = startup_cmd(&Spec {
            exe: PathBuf::from(r"C:\Program Files\ytui\ytui-resolver.exe"),
            args: vec!["--bind".into(), "0.0.0.0:8787".into()],
        });
        assert!(cmd.starts_with("@echo off\r\nstart \"\" /min "));
        assert!(cmd.contains("\"C:\\Program Files\\ytui\\ytui-resolver.exe\""));
        assert!(cmd.contains(" --bind 0.0.0.0:8787"));
    }
}
