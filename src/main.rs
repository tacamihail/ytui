//! A YouTube Music player for the terminal.
//!
//! ratatui frontend. yt-dlp is only used to list and resolve URLs; symphonia
//! decodes the AAC stream and cpal sends it to the sound card.

mod app;
mod audio;
mod auth;
mod config;
mod entry;
mod listcache;
mod media;
mod remote;
mod ui;
mod update;
mod ytdl;

use crossterm::event::{
    DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{execute, ExecutableCommand};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;
use std::sync::mpsc;

fn main() {
    tune_allocator();
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
            "discover" => std::process::exit(discover(&args[1..])),
            "update" => std::process::exit(update::cli(&args[1..])),
            // Internal: the Windows half of the WSL media bridge. Kept out of
            // the help text because users never start it themselves.
            "--media-bridge" => std::process::exit(media::serve_bridge()),
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

/// glibc gives each thread its own 64 MB heap arena, so a handful of threads
/// is enough to make VIRT look like hundreds of megabytes even though almost
/// none of it is resident. Two arenas is plenty for this many threads, and it
/// keeps the virtual size flat instead of jumping whenever a thread allocates.
fn tune_allocator() {
    #[cfg(target_env = "gnu")]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
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
        let _ = io::stdout().execute(DisableFocusChange);
        orig_hook(info);
    }));

    let mut stdout = io::stdout();
    enable_raw_mode()?;
    let _ = execute!(stdout, EnterAlternateScreen, EnableMouseCapture, EnableFocusChange);
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = app::App::run(&mut terminal, &mut app, fetch_rx);

    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture, DisableFocusChange);
    let _ = terminal.show_cursor();

    res
}

fn doctor() -> i32 {
    let mut healthy = true;
    println!("ytui {} environment check\n", env!("CARGO_PKG_VERSION"));

    let cfg = config::load();

    // The resolver is checked first because a healthy one removes the local
    // yt-dlp/deno requirement entirely.
    let remote = remote::settings(&cfg);
    let mut remote_ok = false;
    if let Some(s) = &remote {
        match remote::health(s) {
            Ok((h, ms)) => {
                remote_ok = h.ok;
                if !h.ok {
                    println!("✗ resolver {} reported not ok", s.base);
                    healthy = false;
                } else {
                    let version = if h.version.is_empty() {
                        "?"
                    } else {
                        &h.version
                    };
                    println!("✔ resolver {} (v{version}, {ms} ms)", s.base);
                    if h.cookies {
                        println!(
                            "· resolver uses its own cookies.txt (two machines refreshing one session can sign out)"
                        );
                    }
                }
                match (&h.ytdlp, &h.deno) {
                    (Some(y), Some(d)) => println!("· resolver yt-dlp {y} · {d}"),
                    (Some(y), None) => {
                        println!("✗ resolver yt-dlp {y} · deno missing");
                        healthy = false;
                    }
                    (None, Some(d)) => {
                        println!("✗ resolver deno {d} · yt-dlp missing");
                        healthy = false;
                    }
                    (None, None) => {
                        println!("✗ resolver has neither yt-dlp nor deno");
                        healthy = false;
                    }
                }
            }
            Err(e) if s.fallback => {
                println!(
                    "· resolver {} unavailable ({e}), using local yt-dlp",
                    s.base
                );
            }
            Err(e) => {
                println!("✗ resolver {}: {e}", s.base);
                healthy = false;
            }
        }
    }
    // Local yt-dlp only matters when nothing else can resolve: no resolver at
    // all, or the configured one is down and fallback is enabled.
    let local_needed = match &remote {
        None => true,
        Some(s) => s.fallback && !remote_ok,
    };

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
            } else if local_needed {
                println!("✗ yt-dlp {v} is too old, update it with `yt-dlp -U`");
                healthy = false;
            } else {
                println!("· yt-dlp {v} is old, but the resolver does the work");
            }
        }
        _ if !local_needed => {
            println!("· yt-dlp not needed locally while the resolver is up");
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
        _ if !local_needed => {
            println!("· deno not needed locally while the resolver is up");
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

    match media::Controls::start() {
        Some(controls) => println!("✔ media keys: {}", controls.mode()),
        None => println!(
            "· media keys: {} (could not register right now)",
            media::status()
        ),
    }

    match cfg.cookie_args() {
        Some((flag, value)) => {
            // A file export that lost its first-party cookies was signed out
            // by YouTube; it is not an expiry problem, and running yt-dlp
            // again would only replay the dead session.
            let signed_out = flag == "--cookies"
                && std::fs::read_to_string(&value)
                    .map(|txt| !ytui_resolve::has_session_cookies(&txt))
                    .unwrap_or(false);
            if signed_out {
                println!("✗ cookies: signed out ({value})");
                println!("  {}", ytui_resolve::SIGNED_OUT);
                healthy = false;
            } else {
                // The saved account name can be months old; ask YouTube once so
                // doctor catches cookies that no longer sign in instead of
                // reporting a healthy account.
                match ytdl::account(&cfg) {
                    Ok((name, _)) => {
                        println!("✔ cookies: {value} ({flag})");
                        println!("✔ account: {name}");
                    }
                    Err(e) => {
                        println!("✗ cookies could not be verified: {value} ({flag})");
                        println!("  {e}");
                        healthy = false;
                    }
                }
            }
        }
        None => println!("· cookies: none (search and playback work without them)"),
    }

    let rate = u64::from(audio::output_rate());
    // The decoded ring, then the encoded ranges the reader keeps ready: about
    // a megabyte queued ahead plus two ranges in flight (see http.rs).
    let audio_kib = (cfg.buffer_ms as u64 + 1000) * rate / 1000 * 2 * 4 / 1000
        + 1024
        + 2 * cfg.cache_kib;
    println!(
        "· buffer {} ms · range cache {} KiB · ~{} KiB audio RAM",
        cfg.buffer_ms, cfg.cache_kib, audio_kib
    );

    if healthy {
        println!("\nall good, run `ytui`");
        0
    } else {
        println!("\nsomething is missing, see the ✗ lines above");
        1
    }
}

fn discover(args: &[String]) -> i32 {
    let mut save = false;
    let mut quiet = false;
    let mut timeout = remote::SCAN_BUDGET.as_secs();
    let mut url: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--save" => save = true,
            "-q" | "--quiet" => quiet = true,
            "--url" => {
                i += 1;
                match args.get(i) {
                    Some(u) if u.starts_with("http://") || u.starts_with("https://") => {
                        url = Some(u.clone());
                    }
                    _ => {
                        eprintln!("--url needs an http:// or https:// address");
                        return 2;
                    }
                }
            }
            "--timeout" => {
                i += 1;
                match args.get(i).and_then(|s| s.parse::<u64>().ok()) {
                    Some(n) => timeout = n.max(1),
                    None => {
                        eprintln!("--timeout needs a number of seconds");
                        return 2;
                    }
                }
            }
            other => {
                eprintln!("unknown option: {other}");
                return 2;
            }
        }
        i += 1;
    }

    if let Some(url) = url {
        return use_url(&url, save, quiet);
    }

    if !quiet {
        println!("scanning localhost and private networks for ytui-resolver (up to {timeout}s)…");
    }
    let found = remote::scan_for(std::time::Duration::from_secs(timeout));
    if found.is_empty() {
        if quiet {
            println!("no resolver found; using local yt-dlp");
        } else {
            println!(
                "no resolver found on port {}; ytui will keep using local yt-dlp",
                remote::SCAN_PORT
            );
        }
        return 0;
    }

    if save {
        let mut cfg = config::load();
        // Prefer a tokenless private resolver; fall back to a token-protected
        // one only when a token is already configured.
        let chosen = found.iter().find(|f| !f.auth).or_else(|| {
            if remote::has_token(&cfg) {
                found.first()
            } else {
                None
            }
        });
        match chosen {
            Some(f) => {
                cfg.resolver_url = f.url.clone();
                config::save(&cfg);
                println!("resolver: {} (v{})", f.url, f.version);
            }
            None => {
                println!("resolver found but needs a token; set resolver_token_file");
                for f in &found {
                    println!("  {} v{}", f.url, f.version);
                }
            }
        }
        return 0;
    }

    if quiet {
        println!("{}", found[0].url);
    } else {
        println!("found {} resolver(s):", found.len());
        for f in &found {
            let solve = f
                .solve_ms
                .map(|ms| format!(", ~{:.1}s/track", ms as f64 / 1000.0))
                .unwrap_or_default();
            println!(
                "  {} v{} ({} ms{}){}",
                f.url,
                f.version,
                f.ms,
                solve,
                if f.auth { " · token required" } else { "" }
            );
        }
    }
    0
}

/// Checks a resolver URL given by hand. The scan only sees directly attached
/// private /24s, so a tailnet, VPN or another subnet needs this or `--url`.
fn use_url(url: &str, save: bool, quiet: bool) -> i32 {
    let mut cfg = config::load();
    let s = remote::settings_at(url, &cfg);
    let (health, ms) = match remote::health(&s) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("✗ {}: {e}", s.base);
            return 1;
        }
    };
    if !health.ok {
        eprintln!("✗ {}: resolver is not healthy", s.base);
        return 1;
    }
    if health.auth && !remote::has_token(&cfg) {
        eprintln!(
            "resolver {} needs a token; set resolver_token_file or YTUI_RESOLVER_TOKEN first",
            s.base
        );
        return 1;
    }
    if save {
        cfg.resolver_url = s.base.clone();
        config::save(&cfg);
        println!("resolver: {} (v{}, {} ms)", s.base, health.version, ms);
    } else if quiet {
        println!("{}", s.base);
    } else {
        println!(
            "resolver: {} v{} ({} ms){}",
            s.base,
            health.version,
            ms,
            if health.auth { " · token required" } else { "" }
        );
    }
    0
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
  ytui discover [--save]  scan the LAN for an optional resolver
  ytui discover --url U   check a resolver by address, e.g. over a VPN
  ytui update             pull the latest source and rebuild
  ytui update --check     only report whether newer commits exist
  ytui --version          print version

inside the player: ? for the full key map

media keys (play/pause, next, previous) are handled by the OS media session:
SMTC on Windows, MPRIS on Linux, Now Playing on macOS. In WSL, install the
native Windows build so ytui.exe can serve the Windows session.
",
        env!("CARGO_PKG_VERSION")
    );
}
