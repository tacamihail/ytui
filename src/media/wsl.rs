//! WSL side of the media bridge.
//!
//! Windows delivers media keys to Windows processes, not to the Linux VM, so a
//! WSL build cannot register an SMTC session itself. Instead it starts the
//! native `ytui.exe --media-bridge` (the Windows build, found on the interop
//! PATH) and exchanges JSON lines with it: state goes in, button presses come
//! out.

use super::wire;
use super::{Event, NowPlaying};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

pub struct Backend {
    tx: Sender<NowPlaying>,
    child: Child,
    exe: PathBuf,
}

impl Backend {
    pub fn start(events: Sender<Event>) -> Option<Self> {
        let exe = find_bridge()?;
        let mut child = Command::new(&exe)
            .arg("--media-bridge")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;

        // Read stdout on its own thread and wait for the ready line. Without
        // it, an old ytui.exe (which does not know --media-bridge) would look
        // like a successful bridge that never produces events.
        let (lines_tx, lines_rx) = mpsc::channel::<String>();
        let reader = thread::Builder::new()
            .name("ytui-media-wsl-lines".into())
            .spawn(move || {
                for line in BufReader::new(stdout).lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    if lines_tx.send(line).is_err() {
                        break;
                    }
                }
            });
        if reader.is_err() {
            let _ = child.kill();
            return None;
        }
        match lines_rx.recv_timeout(Duration::from_millis(1500)) {
            Ok(line) if wire::is_ready(&line) => {}
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }

        let (tx, rx) = mpsc::channel::<NowPlaying>();
        let writer = thread::Builder::new()
            .name("ytui-media-wsl".into())
            .spawn(move || {
                let mut out = stdin;
                for now in rx {
                    let line = wire::encode_update(&now);
                    if line.is_empty() || writeln!(out, "{line}").is_err() || out.flush().is_err() {
                        break;
                    }
                }
            });
        if writer.is_err() {
            let _ = child.kill();
            return None;
        }

        let reader = thread::Builder::new()
            .name("ytui-media-wsl-events".into())
            .spawn(move || {
                for line in lines_rx {
                    if let Some(event) = wire::parse_event(&line) {
                        if events.send(event).is_err() {
                            break;
                        }
                    }
                }
            });
        if reader.is_err() {
            let _ = child.kill();
            return None;
        }

        Some(Self { tx, child, exe })
    }

    pub fn mode(&self) -> String {
        format!("WSL bridge via {}", self.exe.display())
    }

    pub fn update(&mut self, now: NowPlaying) {
        let _ = self.tx.send(now);
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        // Closing the writer channel would also end the child, but killing is
        // immediate and keeps a crashed ytui from leaving a hidden ytui.exe
        // behind with a stale media session.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `YTUI_MEDIA_BRIDGE` wins, otherwise the first `ytui.exe` on PATH. In WSL
/// the interop PATH includes the Windows directories, so a normal `install.ps1`
/// installation is found without any setup.
pub fn find_bridge() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("YTUI_MEDIA_BRIDGE") {
        let raw = value.to_string_lossy().into_owned();
        let direct = PathBuf::from(&raw);
        if direct.is_file() {
            return Some(direct);
        }
        if let Some(converted) = windows_path_to_wsl(&raw) {
            if converted.is_file() {
                return Some(converted);
            }
        }
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join("ytui.exe");
        if candidate.is_file() && is_windows_exe(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// Accept `C:\Users\me\bin\ytui.exe` in `YTUI_MEDIA_BRIDGE`, because that is
/// what a user copying a path from PowerShell will paste.
fn windows_path_to_wsl(path: &str) -> Option<PathBuf> {
    let bytes = path.as_bytes();
    if bytes.len() < 3 || bytes[1] != b':' || (bytes[2] != b'\\' && bytes[2] != b'/') {
        return None;
    }
    let drive = (bytes[0] as char).to_ascii_lowercase();
    let rest = path[2..].replace('\\', "/");
    Some(PathBuf::from(format!("/mnt/{drive}{rest}")))
}

fn is_windows_exe(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 2];
    file.read_exact(&mut magic).is_ok() && &magic == b"MZ"
}
