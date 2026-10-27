//! A YouTube Music player for the terminal.
//!
//! ratatui frontend. yt-dlp is only used to list and resolve URLs; symphonia
//! decodes the AAC stream and cpal sends it to the sound card.

mod app;
mod audio;
mod auth;
mod config;
mod entry;
mod ui;
mod ytapi;
mod ytdl;

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::sync::mpsc;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(cmd) = args.first() {
        match cmd.as_str() {
            "--version" | "-V" => {
                println!("ytui {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            "-h" | "--help" => {
                help();
                return;
            }
            "auth" => std::process::exit(auth::cli(&args[1..])),
            "logout" => std::process::exit(auth::logout()),
            "doctor" => std::process::exit(doctor()),
            other => {
                eprintln!("unknown command: {other}\n");
                help();
                std::process::exit(2);
            }
        }
    }
    if let Err(e) = run_tui() {
        eprintln!("ytui: {e}");
        std::process::exit(1);
    }
}

fn run_tui() -> io::Result<()> {
    let cfg = config::load();
    let player = audio::Player::new(&cfg);
    let (fetch_tx, fetch_rx) = mpsc::channel::<ytdl::Msg>();
    let mut app = app::App::new(cfg, player, fetch_tx);

    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = io::stdout().execute(LeaveAlternateScreen);
        let _ = io::stdout().execute(DisableMouseCapture);
        orig_hook(info);
    }));

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    let _ = execute!(stdout, EnterAlternateScreen, EnableMouseCapture);
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = app::App::run(&mut terminal, &mut app, fetch_rx);

    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture);
    let _ = terminal.show_cursor();

    res
}

fn doctor() -> i32 {
    let mut healthy = true;
    println!("ytui {} environment check\n", env!("CARGO_PKG_VERSION"));

    match ytdl::command().arg("--version").output() {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
            let year = v
                .split('.')
                .next()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0);
            if year >= 2025 {
                println!("✔ yt-dlp {v}");
            } else {
                println!("✗ yt-dlp {v} is too old, run install.sh to update it");
                healthy = false;
            }
        }
        _ => {
            println!("✗ yt-dlp not found in PATH (needed for search and stream resolution)");
            healthy = false;
        }
    }

    let deno = std::process::Command::new("deno")
        .env("PATH", ytdl::tool_path())
        .arg("--version")
        .output();
    match deno {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8_lossy(&o.stdout);
            println!("✔ {}", v.lines().next().unwrap_or("deno").trim());
        }
        _ => {
            println!("✗ deno not found, yt-dlp needs it to solve YouTube JS challenges");
            healthy = false;
        }
    }

    match audio::check_device() {
        Ok(name) => println!("✔ audio output: {name}"),
        Err(e) => {
            println!("✗ audio output: {e}");
            healthy = false;
        }
    }

    let cfg = config::load();
    match cfg.cookie_args() {
        Some((flag, value)) => {
            println!("✔ cookies: {value} ({flag})");
            match auth::account() {
                Some(a) => println!("✔ account: {}", a.name),
                None => println!("· account not verified yet, run `ytui auth --status`"),
            }
        }
        None => println!("· cookies: none (search and playback work without them)"),
    }

    println!(
        "· buffer {} ms · range cache {} KiB · ~{} KiB audio RAM",
        cfg.buffer_ms,
        cfg.cache_kib,
        (cfg.buffer_ms as u64 + 1000) * 48 * 2 * 4 / 1000 + cfg.cache_kib
    );

    if healthy {
        println!("\nall good, run `ytui`");
        0
    } else {
        println!("\nsomething is missing, see the ✗ lines above");
        1
    }
}

fn help() {
    println!(
        "\
ytui {}\nA YouTube Music player for the terminal

usage:
  ytui                    open the player
  ytui auth <cookies.txt> connect your YouTube account
  ytui auth --browser X   read cookies from a local browser
  ytui auth --status      show connection status
  ytui logout             forget cookies and account
  ytui doctor             check yt-dlp, audio device and config
  ytui --version          print version

inside the player: ? for the full key map
",
        env!("CARGO_PKG_VERSION")
    );
}
