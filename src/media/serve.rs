//! Windows side of the WSL media bridge. Spawned by the Linux build as
//! `ytui.exe --media-bridge`; owns an SMTC session and speaks JSON lines on
//! stdio. It never touches the terminal.

use super::{windows, wire};
use std::io::{self, BufRead, Write};
use std::sync::mpsc;
use std::thread;

pub fn run() -> i32 {
    let (tx, rx) = mpsc::channel();
    let Some(mut backend) = windows::Backend::start(tx) else {
        return 1;
    };

    // Tell the WSL side that this binary understands the bridge protocol
    // before it starts forwarding state.
    let mut ready = io::stdout();
    if writeln!(ready, "{}", wire::encode_ready()).is_err() || ready.flush().is_err() {
        return 1;
    }
    drop(ready);

    let writer = thread::Builder::new()
        .name("ytui-media-out".into())
        .spawn(move || {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            for event in rx {
                let line = wire::encode_event(&event);
                if line.is_empty() || writeln!(out, "{line}").is_err() || out.flush().is_err() {
                    break;
                }
            }
        });
    if writer.is_err() {
        return 1;
    }

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else {
            break;
        };
        if let Some(now) = wire::parse_update(&line) {
            backend.update(now);
        }
    }
    drop(backend);
    0
}
