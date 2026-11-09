//! Application state, key bindings and mouse handling.

use crate::audio::{self, Cmd, Repeat, State as PlayerState, Status, Track};
use crate::auth;
use crate::config::{self, Config};
use crate::entry::{fmt_f64, Entry};
use crate::listcache;
use crate::remote;
use crate::ui;
use crate::update;
use crate::ytdl::{self, Kind, Msg};
use crossterm::event::{
    poll, read, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::widgets::ListState;
use ratatui::Terminal;
use std::io;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const TABS: [&str; 6] = [
    "Liked Music",
    "Playlists",
    "Subscriptions",
    "History",
    "Queue",
    "Search",
];

pub const TAB_LIKED: usize = 0;
pub const TAB_PLAYLISTS: usize = 1;
pub const TAB_SUBS: usize = 2;
pub const TAB_HISTORY: usize = 3;
pub const TAB_QUEUE: usize = 4;
pub const TAB_SEARCH: usize = 5;

/// Rows one wheel notch moves. One row per notch means dozens of flicks to get
/// anywhere in a few hundred tracks.
const WHEEL_ROWS: i32 = 5;

/// Clickable transport control in the player bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerBtn {
    Prev,
    PlayPause,
    Next,
}

/// Latest known state of the version on github compared with this build. The
/// check is lazy (help or `u` starts it) and runs in the background; the tabs
/// row and the help overlay read the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateState {
    /// Not checked yet: the check is lazy and starts when help is first opened,
    /// so neither launch nor the help overlay ever waits on the network.
    Idle,
    Checking,
    Current,
    /// A newer `[package] version` is on github.
    Available(String),
    Updating,
    Installed,
    Failed(String),
}

impl UpdateState {
    /// One short phrase for the help overlay's `updates` heading.
    pub fn label(&self) -> String {
        match self {
            UpdateState::Idle => "not checked".into(),
            UpdateState::Checking => "checking…".into(),
            UpdateState::Current => "up to date".into(),
            UpdateState::Available(v) => format!("{v} available"),
            UpdateState::Updating => "updating…".into(),
            UpdateState::Installed => "installed, restart ytui".into(),
            UpdateState::Failed(e) => {
                let why = e.lines().next().unwrap_or("").trim();
                if why.is_empty() {
                    "check failed".into()
                } else {
                    format!("check failed: {why}")
                }
            }
        }
    }

    /// Something the redraw loop has to animate.
    pub fn animating(&self) -> bool {
        matches!(self, UpdateState::Checking | UpdateState::Updating)
    }
}

/// Results from the background update worker.
enum UpdMsg {
    /// The newer version, when there is one; `announce` marks a check the user
    /// asked for, whose outcome is toasted.
    Checked(Result<Option<String>, String>, bool),
    Installed(Result<String, String>),
}

#[derive(Default)]
pub enum ScanState {
    #[default]
    Idle,
    Running,
    Done(Vec<remote::Found>),
}

pub struct App {
    pub cfg: Config,
    pub tab: usize,
    pub search: Vec<Entry>,
    pub liked: Vec<Entry>,
    pub playlists: Vec<Entry>,
    pub playlist_tracks: Vec<Entry>,
    pub playlist_title: String,
    pub in_playlist: bool,
    pub subs: Vec<Entry>,
    pub history: Vec<Entry>,
    pub sel: [usize; 6],
    pub list_state: ListState,
    pub list_offset: usize,
    pub loading: Option<(Kind, Instant)>,
    /// Rows received by the fetch in progress, shown in the status bar.
    pub fetch_count: usize,
    /// Identifies the newest fetch; messages from older ones are dropped.
    fetch_seq: u64,
    /// Set while a refresh keeps the cached rows visible: they are replaced by
    /// the first new page instead of being cleared up front.
    pending_clear: Option<Kind>,
    pub toast: Option<(String, Instant, bool)>,
    pub search_open: bool,
    pub search_input: String,
    pub help_open: bool,
    pub scan_open: bool,
    pub scan: ScanState,
    pub scan_sel: usize,
    scan_rx: Option<Receiver<Vec<remote::Found>>>,
    /// Set when the overlay closes, so a 60-second menu scan stops probing.
    scan_cancel: Option<Arc<AtomicBool>>,
    pub last_query: String,
    pub should_quit: bool,
    pub player: Arc<Mutex<PlayerState>>,
    /// Animation clock for spinners; started once so every draw agrees on the
    /// frame and the wheel does not jump when a message arrives.
    pub anim: Instant,
    /// Set by the list while the selected row is scrolling, so the loop keeps
    /// redrawing fast enough for the marquee even when nothing is loading.
    pub marquee: bool,
    /// Whether the terminal reports itself as focused. Terminals without
    /// focus reporting never send FocusLost, so this stays true and the
    /// marquee behaves as it did before.
    pub focused: bool,
    /// Which row is scrolling (tab, index, playlist detail) and since when.
    pub marquee_key: (usize, usize, bool),
    pub marquee_since: Instant,
    pub update: UpdateState,
    /// Clickable update chip in the tabs row, while one is drawn.
    pub update_hit: Option<(u16, u16)>,
    /// `u` pressed while the check was still running: install as soon as it
    /// reports commits to pull, so one press still updates.
    update_wanted: bool,
    upd_tx: Sender<UpdMsg>,
    upd_rx: Receiver<UpdMsg>,
    pub cmd_tx: Sender<Cmd>,
    pub fetch_tx: Sender<Msg>,
    pub account: Option<String>,
    pub last_click: Option<(Instant, usize)>,
    // Geometry captured while drawing, used for mouse hit-testing.
    pub tab_hits: Vec<(u16, u16)>,
    pub player_buttons: Vec<(Rect, PlayerBtn)>,
    pub vol_area: Rect,
    pub mute_area: Rect,
    dragging_volume: bool,
    pub gauge_area: Rect,
    pub list_items_area: Rect,
    // Queue rows are rebuilt only when the queue actually changes.
    pub queue_rows: Vec<(String, bool)>,
    queue_version: u64,
    queue_index: usize,
    /// Last notice sequence number taken from the player, so a skip message
    /// is shown exactly once.
    notice_seq: u64,
    pending_playlist_title: String,
}

impl App {
    pub fn new(cfg: Config, player: audio::Player, fetch_tx: Sender<Msg>) -> Self {
        let history = config::load_json(&config::history_path()).unwrap_or_default();
        let account = auth::account().map(|a| a.name);
        let (upd_tx, upd_rx) = mpsc::channel();
        let mut app = Self {
            cfg,
            tab: TAB_LIKED,
            search: Vec::new(),
            liked: Vec::new(),
            playlists: Vec::new(),
            playlist_tracks: Vec::new(),
            playlist_title: String::new(),
            in_playlist: false,
            subs: Vec::new(),
            history,
            sel: [0; 6],
            list_state: ListState::default(),
            list_offset: 0,
            loading: None,
            fetch_count: 0,
            fetch_seq: 0,
            pending_clear: None,
            toast: None,
            search_open: false,
            search_input: String::new(),
            help_open: false,
            scan_open: false,
            scan: ScanState::Idle,
            scan_sel: 0,
            scan_rx: None,
            scan_cancel: None,
            last_query: String::new(),
            should_quit: false,
            player: Arc::clone(&player.state),
            anim: Instant::now(),
            marquee: false,
            focused: true,
            marquee_key: (usize::MAX, usize::MAX, false),
            marquee_since: Instant::now(),
            update: UpdateState::Idle,
            update_hit: None,
            update_wanted: false,
            upd_tx: upd_tx.clone(),
            upd_rx,
            cmd_tx: player.tx_clone(),
            fetch_tx,
            account,
            last_click: None,
            tab_hits: Vec::new(),
            player_buttons: Vec::new(),
            vol_area: Rect::default(),
            mute_area: Rect::default(),
            dragging_volume: false,
            gauge_area: Rect::default(),
            list_items_area: Rect::default(),
            queue_rows: Vec::new(),
            queue_version: 0,
            queue_index: usize::MAX,
            notice_seq: 0,
            pending_playlist_title: String::new(),
        };
        // Opening the player on Liked Music loads it right away.
        app.set_tab(TAB_LIKED);
        app
    }

    // ---------------- main loop ----------------

    pub fn run(
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
        app: &mut App,
        fetch_rx: Receiver<Msg>,
    ) -> io::Result<()> {
        loop {
            app.sync_queue_cache();
            app.pick_up_notice();
            app.poll_scan();
            terminal.draw(|f| ui::draw(f, app))?;
            // While a list is streaming in, a track is being resolved or the
            // selected row is scrolling its title, redraw often enough that
            // the animation turns; when idle there is nothing to animate.
            let resolving = app
                .player
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .status
                == Status::Loading;
            let wait = if app.loading.is_some()
                || resolving
                || app.marquee
                || app.scan_running()
                || app.update.animating()
            {
                Duration::from_millis(33)
            } else {
                Duration::from_millis(120)
            };
            if poll(wait)? {
                match read()? {
                    Event::Key(k) => {
                        if k.kind != KeyEventKind::Release {
                            app.on_key(k);
                        }
                    }
                    Event::Mouse(m) => app.on_mouse(m),
                    Event::FocusGained => app.focused = true,
                    Event::FocusLost => app.focused = false,
                    _ => {}
                }
            }
            while let Ok(m) = fetch_rx.try_recv() {
                app.on_fetch(m);
            }
            while let Ok(m) = app.upd_rx.try_recv() {
                app.on_update(m);
            }
            app.expire_toasts();
            if app.should_quit {
                return Ok(());
            }
        }
    }

    fn sync_queue_cache(&mut self) {
        let (version, index, rows, resolved) = {
            let st = lock(&self.player);
            let changed = st.version != self.queue_version || st.index != self.queue_index;
            let rows = if changed {
                Some(
                    st.queue
                        .iter()
                        .enumerate()
                        .map(|(i, t)| {
                            // A pasted link is still unnamed until yt-dlp
                            // resolves it; show the same placeholder the player
                            // uses instead of a blank line in the queue.
                            let title = if t.title.trim().is_empty() {
                                "…".to_string()
                            } else {
                                t.title.clone()
                            };
                            (title, i == st.index)
                        })
                        .collect::<Vec<_>>(),
                )
            } else {
                None
            };
            // A pasted link only learns its real title once yt-dlp resolved it.
            let resolved = if changed {
                st.queue
                    .get(st.index)
                    .filter(|t| !t.title.is_empty())
                    .map(|t| (t.url.clone(), t.title.clone()))
            } else {
                None
            };
            (st.version, st.index, rows, resolved)
        };
        self.queue_version = version;
        self.queue_index = index;
        if let Some(rows) = rows {
            self.queue_rows = rows;
        }
        if let Some((url, title)) = resolved {
            let mut dirty = false;
            for e in self.history.iter_mut() {
                if e.title.is_empty() && e.url() == url {
                    e.title = title.clone();
                    dirty = true;
                }
            }
            if dirty {
                config::save_json(&config::history_path(), &self.history);
            }
        }
    }

    // ---------------- messages ----------------

    pub fn on_fetch(&mut self, msg: Msg) {
        match msg {
            Msg::Begin(seq, kind) => {
                if seq != self.fetch_seq {
                    return;
                }
                self.loading = Some((kind, Instant::now()));
                self.fetch_count = 0;
                if kind == Kind::PlaylistTracks {
                    self.playlist_tracks.clear();
                    self.in_playlist = true;
                    self.playlist_title = self.pending_playlist_title.clone();
                    self.sel[TAB_PLAYLISTS] = 0;
                    self.pending_clear = None;
                } else if self.list_mut(kind).is_empty() {
                    self.pending_clear = None;
                } else {
                    // Keep the cached rows visible until the first new page
                    // arrives, so a refresh never flashes an empty tab.
                    self.pending_clear = Some(kind);
                }
            }
            Msg::Add(seq, kind, entry) => {
                if seq != self.fetch_seq {
                    return;
                }
                if self.pending_clear == Some(kind) {
                    self.pending_clear = None;
                    self.list_mut(kind).clear();
                    self.sel[self.tab_of(kind)] = 0;
                }
                if kind == Kind::PlaylistTracks {
                    self.playlist_tracks.push(entry);
                } else {
                    self.list_mut(kind).push(entry);
                }
                self.fetch_count += 1;
            }
            Msg::Done(seq, kind, label) => {
                if seq != self.fetch_seq {
                    return;
                }
                if self.pending_clear.take() == Some(kind) {
                    // The refresh came back empty; keeping the old rows would
                    // show tracks the account no longer has.
                    self.list_mut(kind).clear();
                    self.sel[self.tab_of(kind)] = 0;
                }
                self.loading = None;
                self.clamp_sel();
                self.save_cache(kind);
                self.set_toast(format!("✔ {label}"), false);
            }
            Msg::Error(seq, kind, err) => {
                if seq != self.fetch_seq {
                    return;
                }
                // A failed refresh keeps the cached rows; the toast explains
                // why they may be stale.
                self.pending_clear = None;
                self.loading = None;
                if kind == Kind::PlaylistTracks {
                    self.in_playlist = false;
                }
                self.set_toast(err, true);
            }
        }
    }

    pub fn expire_toasts(&mut self) {
        let expired =
            matches!(&self.toast, Some((_, t, _)) if t.elapsed() > Duration::from_secs(5));
        if expired {
            self.toast = None;
        }
    }

    // ---------------- updates ----------------

    /// One key for the whole flow: install a pending update, otherwise check.
    /// Bound to `u`, in the main view and inside the help overlay alike.
    pub fn update_now(&mut self) {
        match self.update {
            UpdateState::Available(_) => self.install_update(),
            UpdateState::Checking => self.update_wanted = true,
            UpdateState::Idle | UpdateState::Current | UpdateState::Failed(_) => {
                self.check_updates(true)
            }
            UpdateState::Updating | UpdateState::Installed => {}
        }
    }

    /// Compare this build's version with github on a worker thread. Opening
    /// help starts a check every time, and `u` refreshes it when there is
    /// nothing to install.
    pub fn check_updates(&mut self, announce: bool) {
        if matches!(self.update, UpdateState::Updating | UpdateState::Checking) {
            return;
        }
        self.update = UpdateState::Checking;
        if announce {
            self.set_toast("checking github for updates…", false);
        }
        let tx = self.upd_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(UpdMsg::Checked(update::available(), announce));
        });
    }

    /// Run `ytui update` in a child process: its cargo output must not land in
    /// the alternate screen, and a failed build leaves the running binary be.
    pub fn install_update(&mut self) {
        if matches!(self.update, UpdateState::Updating | UpdateState::Checking) {
            return;
        }
        self.update = UpdateState::Updating;
        self.set_toast("updating: pulling and building, this takes a minute…", false);
        let tx = self.upd_tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(UpdMsg::Installed(run_update()));
        });
    }

    fn on_update(&mut self, msg: UpdMsg) {
        match msg {
            UpdMsg::Checked(result, announce) => {
                // A `u` pressed while the check ran still updates in one go.
                let wanted = std::mem::take(&mut self.update_wanted);
                let announce = announce || wanted;
                match result {
                    Ok(None) => {
                        self.update = UpdateState::Current;
                        if announce {
                            self.set_toast("✔ already up to date", false);
                        }
                    }
                    Ok(Some(v)) => {
                        self.update = UpdateState::Available(v.clone());
                        if wanted {
                            self.install_update();
                        } else if announce {
                            self.set_toast(
                                format!("ytui {v} is available on github, press u to update"),
                                false,
                            );
                        }
                    }
                    Err(e) => {
                        self.update = UpdateState::Failed(e.clone());
                        if announce {
                            self.set_toast(e, true);
                        }
                    }
                }
            }
            UpdMsg::Installed(Ok(msg)) => {
                // The CLI also reports "already up to date" or "nothing to
                // pull" with a success status; only an actual install needs a
                // restart.
                self.update = if msg.contains("✔ updated") {
                    UpdateState::Installed
                } else {
                    UpdateState::Current
                };
                self.set_toast(msg, false);
            }
            UpdMsg::Installed(Err(e)) => {
                self.update = UpdateState::Failed(e.clone());
                self.set_toast(e, true);
            }
        }
    }

    /// The engine cannot reach the toast directly, so it bumps a counter when
    /// it walks past a track YouTube would not serve.
    fn pick_up_notice(&mut self) {
        let (seq, msg, err) = {
            let st = lock(&self.player);
            (st.notice_seq, st.notice.clone(), st.notice_err)
        };
        if seq != self.notice_seq {
            self.notice_seq = seq;
            if let Some(msg) = msg {
                self.set_toast(msg, err);
            }
        }
    }

    // ---------------- resolver scan ----------------

    /// Scans localhost and the private networks on port 8787 in the
    /// background, for up to a minute. The result is applied automatically when
    /// it is a tokenless private resolver (or when a token is already
    /// configured).
    fn start_scan(&mut self) {
        self.scan_open = true;
        if self.scan_rx.is_some() {
            return;
        }
        self.scan = ScanState::Running;
        self.scan_sel = 0;
        let (tx, rx) = mpsc::channel();
        self.scan_rx = Some(rx);
        let cancel = Arc::new(AtomicBool::new(false));
        self.scan_cancel = Some(Arc::clone(&cancel));
        std::thread::spawn(move || {
            let found =
                remote::scan_for_cancellable(remote::MENU_SCAN_BUDGET, Some(cancel.as_ref()));
            let _ = tx.send(found);
        });
    }

    /// Stops the worker behind the overlay; a closed scan must not keep the
    /// network busy for the rest of its minute.
    fn cancel_scan(&mut self) {
        if let Some(cancel) = self.scan_cancel.take() {
            cancel.store(true, Ordering::Relaxed);
        }
        self.scan_rx = None;
        self.scan = ScanState::Idle;
    }

    pub fn scan_running(&self) -> bool {
        matches!(self.scan, ScanState::Running)
    }

    fn poll_scan(&mut self) {
        let result = self.scan_rx.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(found) => Some(found),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Vec::new()),
        });
        let Some(found) = result else {
            return;
        };
        self.scan_rx = None;
        self.scan_cancel = None;
        self.scan_sel = 0;
        self.scan = ScanState::Done(found);
        if let Some(chosen) = pick_resolver(&self.scan, &self.cfg) {
            self.apply_resolver(chosen);
        } else if self.scan_open {
            self.set_toast(
                "no tokenless resolver found; still using local yt-dlp",
                false,
            );
        }
    }

    fn scan_move(&mut self, delta: isize) {
        let ScanState::Done(found) = &self.scan else {
            return;
        };
        if found.is_empty() {
            return;
        }
        self.scan_sel = if delta < 0 {
            self.scan_sel.saturating_sub(1)
        } else {
            (self.scan_sel + 1).min(found.len() - 1)
        };
    }

    fn use_scan_result(&mut self) {
        let ScanState::Done(found) = &self.scan else {
            return;
        };
        let Some(chosen) = found.get(self.scan_sel).cloned() else {
            return;
        };
        if chosen.auth && !remote::has_token(&self.cfg) {
            self.set_toast("that resolver needs a token; set resolver_token_file", true);
            return;
        }
        self.apply_resolver(chosen);
    }

    fn apply_resolver(&mut self, chosen: remote::Found) {
        self.cfg.resolver_url = chosen.url.clone();
        config::save(&self.cfg);
        self.cmd(Cmd::SetConfig(self.cfg.clone()));
        self.scan_open = false;
        self.help_open = false;
        self.cancel_scan();
        self.set_toast(
            format!(
                "resolver: {} · {}",
                remote::reach(&chosen.url).label(),
                chosen.url
            ),
            false,
        );
    }

    // ---------------- keys ----------------

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(k.code, KeyCode::Char('c') | KeyCode::Char('d')) {
                self.should_quit = true;
            }
            return;
        }
        if self.scan_open {
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    // Dropping the receiver stops this scan from changing the
                    // config after the overlay was dismissed, and the flag
                    // stops the probes themselves.
                    self.scan_open = false;
                    self.cancel_scan();
                }
                KeyCode::Char('s') => self.start_scan(),
                KeyCode::Up | KeyCode::Char('k') => self.scan_move(-1),
                KeyCode::Down | KeyCode::Char('j') => self.scan_move(1),
                KeyCode::Enter => self.use_scan_result(),
                _ => {}
            }
            return;
        }
        if self.search_open {
            self.on_key_search(k);
            return;
        }
        if self.help_open {
            // `u` works from inside the overlay so the update can be started
            // while the status is being watched; `s` starts a resolver scan.
            match k.code {
                KeyCode::Char('u') => self.update_now(),
                KeyCode::Char('s') => {
                    self.help_open = false;
                    self.scan_open = true;
                    self.start_scan();
                }
                _ => self.help_open = false,
            }
            return;
        }

        match k.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.should_quit = true,
            KeyCode::Char('?') => {
                self.help_open = true;
                // Every look at the help refreshes the status; the overlay
                // opens at once and the heading spins until github answers.
                self.check_updates(false);
            }
            KeyCode::Char('u') => self.update_now(),
            KeyCode::Char('/') => {
                self.search_open = true;
                self.search_input.clear();
            }
            KeyCode::Char(c @ '1'..='6') => {
                let t = c.to_digit(10).unwrap() as usize - 1;
                self.set_tab(t);
            }
            KeyCode::Tab => self.set_tab((self.tab + 1) % 6),
            KeyCode::BackTab => self.set_tab((self.tab + 5) % 6),
            KeyCode::Esc => {
                if self.in_playlist {
                    self.in_playlist = false;
                    self.sel[TAB_PLAYLISTS] = 0;
                }
            }
            KeyCode::Char('r') => self.refresh(),
            KeyCode::Char('x') => {
                self.cmd(Cmd::Stop);
                self.set_toast("stopped", false);
            }
            KeyCode::Char(' ') | KeyCode::Char('p') => {
                self.cmd(Cmd::Toggle);
            }
            KeyCode::Char('m') => {
                let muted = lock(&self.player).muted;
                self.cmd(Cmd::Mute(!muted));
                self.set_toast(if muted { "unmuted" } else { "muted" }, false);
            }
            KeyCode::Char('s') => {
                let on = !lock(&self.player).shuffle;
                self.cmd(Cmd::Shuffle(on));
                self.set_toast(if on { "shuffle: on" } else { "shuffle: off" }, false);
            }
            KeyCode::Char('R') => {
                let next = match lock(&self.player).repeat {
                    Repeat::Off => Repeat::All,
                    Repeat::All => Repeat::One,
                    Repeat::One => Repeat::Off,
                };
                self.cmd(Cmd::Repeat(next));
                let label = match next {
                    Repeat::Off => "repeat: off",
                    Repeat::All => "repeat: all",
                    Repeat::One => "repeat: one",
                };
                self.set_toast(label, false);
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_sel(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_sel(1),
            KeyCode::PageUp => self.move_sel(-10),
            KeyCode::PageDown => self.move_sel(10),
            KeyCode::Home | KeyCode::Char('g') => self.sel[self.tab] = 0,
            KeyCode::End | KeyCode::Char('G') => {
                let len = self.current_len();
                self.sel[self.tab] = len.saturating_sub(1);
            }
            KeyCode::Left | KeyCode::Char('h') => self.seek(-self.cfg.seek_step),
            KeyCode::Right | KeyCode::Char('l') => self.seek(self.cfg.seek_step),
            KeyCode::Char(',') => self.seek(-30.0),
            KeyCode::Char('.') => self.seek(30.0),
            KeyCode::Char('[') => self.change_volume(-self.cfg.volume_step),
            KeyCode::Char(']') => self.change_volume(self.cfg.volume_step),
            KeyCode::Char('>') => {
                self.cmd(Cmd::Next);
            }
            KeyCode::Char('<') => {
                self.cmd(Cmd::Prev);
            }
            KeyCode::Enter => self.activate(),
            KeyCode::Char('a') => self.queue_append(),
            KeyCode::Char('d') => self.delete(),
            KeyCode::Char('c') => self.copy_link(),
            _ => {}
        }
    }

    fn on_key_search(&mut self, k: KeyEvent) {
        match k.code {
            KeyCode::Enter => self.submit_search(),
            KeyCode::Esc => self.search_open = false,
            KeyCode::Backspace => {
                self.search_input.pop();
            }
            KeyCode::Char(c) => {
                if !k.modifiers.contains(KeyModifiers::CONTROL) {
                    self.search_input.push(c);
                }
            }
            _ => {}
        }
    }

    // ---------------- mouse ----------------

    pub fn on_mouse(&mut self, m: MouseEvent) {
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.search_open || self.help_open {
                    self.search_open = false;
                    self.help_open = false;
                    return;
                }
                if m.row == 0 {
                    if let Some((x0, x1)) = self.update_hit {
                        if m.column >= x0 && m.column <= x1 {
                            // Available installs, updating/installed has
                            // nothing left to do; update_now guards those.
                            self.update_now();
                            return;
                        }
                    }
                    for (i, &(x0, x1)) in self.tab_hits.iter().enumerate() {
                        if m.column >= x0 && m.column <= x1 {
                            self.set_tab(i);
                            return;
                        }
                    }
                }
                let btn = self
                    .player_buttons
                    .iter()
                    .find(|(r, _)| m.row == r.y && m.column >= r.x && m.column < r.x + r.width)
                    .map(|(_, b)| *b);
                if let Some(b) = btn {
                    match b {
                        PlayerBtn::Prev => self.cmd(Cmd::Prev),
                        PlayerBtn::PlayPause => self.cmd(Cmd::Toggle),
                        PlayerBtn::Next => self.cmd(Cmd::Next),
                    }
                    return;
                }
                let mi = self.mute_area;
                if mi.width > 0 && m.row == mi.y && m.column >= mi.x && m.column < mi.x + mi.width {
                    let muted = lock(&self.player).muted;
                    self.cmd(Cmd::Mute(!muted));
                    return;
                }
                let v = self.vol_area;
                if v.width > 0 && m.row == v.y && m.column >= v.x && m.column < v.x + v.width {
                    self.dragging_volume = true;
                    self.set_volume_at(m.column);
                    return;
                }
                let g = self.gauge_area;
                if g.width > 0 && m.row == g.y && m.column >= g.x && m.column < g.x + g.width {
                    let dur = lock(&self.player).dur;
                    if dur > 0.0 {
                        let frac = (m.column - g.x) as f64 / g.width as f64;
                        self.cmd(Cmd::SeekAbs(frac * dur));
                        self.set_toast(format!("seek to {}", fmt_f64(frac * dur)), false);
                    }
                    return;
                }
                let a = self.list_items_area;
                if a.height > 0
                    && m.row >= a.y
                    && m.row < a.y + a.height
                    && m.column >= a.x
                    && m.column < a.x + a.width
                {
                    let len = self.current_len();
                    let idx = self.list_offset + (m.row - a.y) as usize;
                    if idx < len {
                        self.sel[self.tab] = idx;
                        let now = Instant::now();
                        let dbl = matches!(self.last_click, Some((t, i)) if t.elapsed() < Duration::from_millis(500) && i == idx);
                        self.last_click = Some((now, idx));
                        if dbl {
                            self.last_click = None;
                            self.activate();
                        } else {
                            self.set_toast("double-click (or ENTER) to play", false);
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => self.move_sel(-WHEEL_ROWS),
            MouseEventKind::ScrollDown => self.move_sel(WHEEL_ROWS),
            MouseEventKind::Drag(MouseButton::Left) => {
                if self.dragging_volume {
                    self.set_volume_at(m.column);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.dragging_volume = false,
            _ => {}
        }
    }

    fn set_volume_at(&mut self, col: u16) {
        let a = self.vol_area;
        if a.width < 2 {
            return;
        }
        let rel = col.saturating_sub(a.x).min(a.width - 1);
        let frac = f64::from(rel) / f64::from(a.width - 1);
        let vol = (frac * 100.0).round() as i64;
        self.cmd(Cmd::Volume(vol));
        // Touching the slider is an explicit "make it audible" gesture.
        if lock(&self.player).muted {
            self.cmd(Cmd::Mute(false));
        }
    }

    // ---------------- tabs & fetching ----------------

    fn set_tab(&mut self, tab: usize) {
        self.tab = tab;
        self.list_offset = 0;
        *self.list_state.offset_mut() = 0;
        self.clamp_sel();
        if self.loading.is_some() {
            return;
        }
        match tab {
            TAB_LIKED if self.liked.is_empty() => {
                // A fresh cache paints instantly and skips the network; a
                // stale one paints too, with a refresh behind it.
                if self.load_cached(Kind::LikedMusic) != Some(true) {
                    self.fetch_liked(false);
                }
            }
            TAB_PLAYLISTS if self.playlists.is_empty() => {
                if self.load_cached(Kind::Playlists) != Some(true) {
                    self.fetch_playlists(false);
                }
            }
            TAB_SUBS if self.subs.is_empty() => {
                if self.load_cached(Kind::Subscriptions) != Some(true) {
                    self.fetch_subs(false);
                }
            }
            _ => {}
        }
    }

    fn refresh(&mut self) {
        match self.tab {
            TAB_SEARCH => {
                if self.last_query.is_empty() {
                    self.search_open = true;
                } else {
                    let q = self.last_query.clone();
                    self.start_fetch(
                        Kind::Search,
                        format!("ytsearch{}:{}", self.cfg.results_count, q),
                    );
                }
            }
            TAB_LIKED => self.fetch_liked(true),
            TAB_PLAYLISTS => {
                if self.in_playlist {
                    self.fetch_playlist_tracks();
                } else {
                    self.fetch_playlists(true);
                }
            }
            TAB_SUBS => self.fetch_subs(true),
            TAB_HISTORY => {
                self.history = config::load_json(&config::history_path()).unwrap_or_default();
                self.clamp_sel();
                self.set_toast("history reloaded", false);
            }
            TAB_QUEUE => self.set_toast("queue is live", false),
            _ => {}
        }
    }

    fn fetch_liked(&mut self, notify: bool) {
        if !self.require_cookies(notify) {
            return;
        }
        self.start_fetch(Kind::LikedMusic, ytdl::LIKED_MUSIC_URL.to_string());
    }

    fn fetch_playlists(&mut self, notify: bool) {
        if !self.require_cookies(notify) {
            return;
        }
        let Some(account) = auth::account() else {
            if notify {
                self.set_toast("account unknown, run: ytui auth <cookies.txt>", true);
            }
            return;
        };
        if account.channel_id.is_empty() {
            if notify {
                self.set_toast("account has no channel id, re-run: ytui auth", true);
            }
            return;
        }
        let url = format!(
            "https://www.youtube.com/channel/{}/playlists",
            account.channel_id
        );
        self.in_playlist = false;
        self.start_fetch(Kind::Playlists, url);
    }

    fn fetch_subs(&mut self, notify: bool) {
        if !self.require_cookies(notify) {
            return;
        }
        self.start_fetch(Kind::Subscriptions, "feed/subscriptions".to_string());
    }

    fn fetch_playlist_tracks(&mut self) {
        let Some(entry) = self.playlists.get(self.sel[TAB_PLAYLISTS]).cloned() else {
            return;
        };
        self.pending_playlist_title = entry.title.clone();
        self.start_fetch(Kind::PlaylistTracks, entry.url());
    }

    fn require_cookies(&mut self, notify: bool) -> bool {
        if self.cfg.has_cookies() {
            return true;
        }
        if notify {
            self.set_toast("not connected, run: ytui auth <cookies.txt>", true);
        }
        false
    }

    fn start_fetch(&mut self, kind: Kind, target: String) {
        self.fetch_seq = self.fetch_seq.wrapping_add(1);
        self.fetch_count = 0;
        self.pending_clear = None;
        self.loading = Some((kind, Instant::now()));
        ytdl::spawn(
            self.fetch_seq,
            kind,
            target,
            self.cfg.clone(),
            self.fetch_tx.clone(),
        );
    }

    /// Paints the last saved listing for `kind` without touching the network.
    /// `Some(fresh)` means the cache was used; `false` means a refresh should
    /// follow, `Some(true)` means it is recent enough to keep.
    fn load_cached(&mut self, kind: Kind) -> Option<bool> {
        let key = listcache::key(kind)?;
        let cached = listcache::load(key, &listcache::stamp(&self.cfg))?;
        // An empty list is never trusted for long: it may just mean the last
        // fetch ran while signed out.
        let fresh = cached.fresh() && !cached.entries.is_empty();
        *self.list_mut(kind) = cached.entries;
        self.clamp_sel();
        Some(fresh)
    }

    fn save_cache(&self, kind: Kind) {
        let Some(key) = listcache::key(kind) else {
            return;
        };
        let entries = match kind {
            Kind::LikedMusic => &self.liked,
            Kind::Playlists => &self.playlists,
            Kind::Subscriptions => &self.subs,
            Kind::Search | Kind::PlaylistTracks => return,
        };
        listcache::save(key, &listcache::stamp(&self.cfg), entries);
    }

    fn submit_search(&mut self) {
        let q = self.search_input.trim().to_string();
        self.search_open = false;
        self.search_input.clear();
        if q.is_empty() {
            return;
        }
        if q.starts_with("http://") || q.starts_with("https://") {
            if q.contains("list=") {
                self.pending_playlist_title = "pasted playlist".into();
                self.set_tab(TAB_PLAYLISTS);
                self.start_fetch(Kind::PlaylistTracks, q);
            } else {
                let url = match q.find("&list=") {
                    Some(pos) => q[..pos].to_string(),
                    None => q.clone(),
                };
                let e = Entry {
                    id: url.clone(),
                    title: String::new(),
                    channel: String::new(),
                    duration: 0,
                    url_override: url,
                };
                let tracks = vec![entry_to_track(&e)];
                self.cmd(Cmd::Play(tracks, 0));
                self.push_history(e);
                self.set_toast("▶ pasted link, resolving…", false);
            }
        } else {
            self.last_query = q.clone();
            self.set_tab(TAB_SEARCH);
            self.start_fetch(
                Kind::Search,
                format!("ytsearch{}:{}", self.cfg.results_count, q),
            );
        }
    }

    // ---------------- playback ----------------

    fn activate(&mut self) {
        if self.tab == TAB_QUEUE {
            let (len, cur, failed) = {
                let st = lock(&self.player);
                (st.queue.len(), st.index, st.status == Status::Error)
            };
            let i = self.sel[TAB_QUEUE];
            if i < len {
                if i == cur && !failed {
                    self.set_toast("already playing", false);
                } else {
                    self.cmd(Cmd::PlayIndex(i));
                }
            }
            return;
        }
        if self.tab == TAB_PLAYLISTS && !self.in_playlist {
            self.fetch_playlist_tracks();
            return;
        }
        let list = self.current_vec().clone();
        let idx = self.sel[self.tab];
        let Some(entry) = list.get(idx) else { return };
        let tracks: Vec<Track> = list.iter().map(entry_to_track).collect();
        self.cmd(Cmd::Play(tracks, idx));
        self.push_history(entry.clone());
        self.set_toast(format!("▶ {}", entry.title), false);
    }

    fn queue_append(&mut self) {
        if self.tab == TAB_QUEUE {
            self.set_toast("already in the queue, press ENTER to play it", false);
            return;
        }
        if let Some(e) = self.current_entry() {
            self.cmd(Cmd::Append(entry_to_track(&e)));
            self.push_history(e.clone());
            self.set_toast(format!("+ queue: {}", e.title), false);
        }
    }

    fn seek(&mut self, delta: f64) {
        let st = lock(&self.player);
        let live = matches!(st.status, Status::Playing | Status::Paused);
        drop(st);
        if live {
            self.cmd(Cmd::Seek(delta));
        }
    }

    fn change_volume(&mut self, delta: i64) {
        let cur = lock(&self.player).volume;
        let next = (cur + delta).clamp(0, 100);
        self.cmd(Cmd::Volume(next));
        self.set_toast(format!("volume: {next}%"), false);
    }

    fn delete(&mut self) {
        match self.tab {
            TAB_HISTORY => {
                if self.sel[TAB_HISTORY] < self.history.len() {
                    self.history.remove(self.sel[TAB_HISTORY]);
                    config::save_json(&config::history_path(), &self.history);
                    self.clamp_sel();
                }
            }
            TAB_QUEUE => {
                let (len, cur) = {
                    let st = lock(&self.player);
                    (st.queue.len(), st.index)
                };
                let i = self.sel[TAB_QUEUE];
                if i < len {
                    if i == cur {
                        self.set_toast("cannot remove the playing track, press x to stop", true);
                    } else {
                        self.cmd(Cmd::RemoveIndex(i));
                    }
                }
            }
            _ => {}
        }
    }

    fn copy_link(&mut self) {
        let Some(e) = self.current_entry() else {
            return;
        };
        let url = e.url();
        for (bin, args) in [
            ("wl-copy", vec![]),
            ("xclip", vec!["-selection", "clipboard"]),
            ("xsel", vec!["--clipboard", "--input"]),
            ("clip.exe", vec![]),
        ] {
            if which(bin) {
                if let Ok(mut ch) = std::process::Command::new(bin)
                    .args(&args)
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                {
                    if let Some(si) = ch.stdin.as_mut() {
                        let _ = std::io::Write::write_all(si, url.as_bytes());
                    }
                    ch.stdin.take();
                    let _ = ch.wait();
                    self.set_toast(format!("copied: {url}"), false);
                    return;
                }
            }
        }
        self.set_toast("no clipboard tool (install wl-clipboard or xclip)", true);
    }

    fn push_history(&mut self, e: Entry) {
        self.history.retain(|x| x.id != e.id);
        self.history.insert(0, e);
        self.history.truncate(self.cfg.history_limit);
        config::save_json(&config::history_path(), &self.history);
    }

    // ---------------- helpers ----------------

    fn list_mut(&mut self, kind: Kind) -> &mut Vec<Entry> {
        match kind {
            Kind::LikedMusic => &mut self.liked,
            Kind::Playlists => &mut self.playlists,
            Kind::Subscriptions => &mut self.subs,
            Kind::Search | Kind::PlaylistTracks => &mut self.search,
        }
    }

    fn tab_of(&self, kind: Kind) -> usize {
        match kind {
            Kind::LikedMusic => TAB_LIKED,
            Kind::Playlists => TAB_PLAYLISTS,
            Kind::Subscriptions => TAB_SUBS,
            Kind::Search | Kind::PlaylistTracks => TAB_SEARCH,
        }
    }

    pub fn current_vec(&self) -> &Vec<Entry> {
        match self.tab {
            TAB_SEARCH => &self.search,
            TAB_LIKED => &self.liked,
            TAB_PLAYLISTS => {
                if self.in_playlist {
                    &self.playlist_tracks
                } else {
                    &self.playlists
                }
            }
            TAB_SUBS => &self.subs,
            TAB_HISTORY => &self.history,
            _ => &self.history,
        }
    }

    pub fn tab_count(&self, i: usize) -> usize {
        match i {
            TAB_SEARCH => self.search.len(),
            TAB_LIKED => self.liked.len(),
            TAB_PLAYLISTS => {
                if self.in_playlist {
                    self.playlist_tracks.len()
                } else {
                    self.playlists.len()
                }
            }
            TAB_SUBS => self.subs.len(),
            TAB_HISTORY => self.history.len(),
            _ => lock(&self.player).queue.len(),
        }
    }

    pub fn current_len(&self) -> usize {
        if self.tab == TAB_QUEUE {
            lock(&self.player).queue.len()
        } else {
            self.current_vec().len()
        }
    }

    pub fn current_entry(&self) -> Option<Entry> {
        if self.tab == TAB_QUEUE {
            return None;
        }
        self.current_vec().get(self.sel[self.tab]).cloned()
    }

    fn move_sel(&mut self, delta: i32) {
        let len = self.current_len();
        if len == 0 {
            return;
        }
        let cur = self.sel[self.tab] as i32;
        self.sel[self.tab] = (cur + delta).clamp(0, len as i32 - 1) as usize;
    }

    fn clamp_sel(&mut self) {
        let len = self.current_len();
        if self.sel[self.tab] >= len {
            self.sel[self.tab] = len.saturating_sub(1);
        }
    }

    fn set_toast(&mut self, msg: impl Into<String>, err: bool) {
        self.toast = Some((msg.into(), Instant::now(), err));
    }

    /// The audio thread may be gone (device error, shutdown); commands are
    /// best-effort and never block the UI.
    fn cmd(&mut self, c: Cmd) {
        let _ = self.cmd_tx.send(c);
    }
}

fn entry_to_track(e: &Entry) -> Track {
    Track {
        title: e.title.clone(),
        channel: e.channel.clone(),
        url: e.url(),
        duration: e.duration,
    }
}

fn lock(p: &Arc<Mutex<PlayerState>>) -> std::sync::MutexGuard<'_, PlayerState> {
    p.lock().unwrap_or_else(|e| e.into_inner())
}

/// The scan is already sorted tokenless-first/fastest-first. Use a tokenless
/// result directly; a token-protected one only when the client already has a
/// token to send.
fn pick_resolver(scan: &ScanState, cfg: &Config) -> Option<remote::Found> {
    let ScanState::Done(found) = scan else {
        return None;
    };
    found.iter().find(|f| !f.auth).cloned().or_else(|| {
        if remote::has_token(cfg) {
            found.first().cloned()
        } else {
            None
        }
    })
}

/// Runs the `ytui update` CLI and distills its output into one line for the
/// toast: the `✔` result on success, the `✗` reason on failure.
fn run_update() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot find the ytui binary: {e}"))?;
    let out = Command::new(exe)
        .arg("update")
        .output()
        .map_err(|e| format!("cannot run ytui update: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        Ok(marked_line(&stdout, '✔').unwrap_or_else(|| "✔ update finished".into()))
    } else {
        Err(marked_line(&stderr, '✗')
            .or_else(|| marked_line(&stdout, '✗'))
            .unwrap_or_else(|| "update failed".into()))
    }
}

/// First line of `text` starting with `mark`, trimmed. The update CLI prefixes
/// its result and error lines with ✔ and ✗.
fn marked_line(text: &str, mark: char) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|l| l.starts_with(mark))
        .map(str::to_string)
}

fn which(bin: &str) -> bool {
    // `split_paths` is the only correct separator-aware split: Windows uses
    // `;`, Unix uses `:`. On Windows the file on disk is `clip.exe`.
    let Ok(paths) = std::env::var("PATH") else {
        return false;
    };
    let exe = format!("{bin}{}", std::env::consts::EXE_SUFFIX);
    std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file() || dir.join(&exe).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_cli_result_is_the_marked_line() {
        let out = "· fetching github.com/tacamihail/ytui ...\n\
                   · fast-forwarding 992bb7c -> 108d1f0\n\
                   ✔ updated to 108d1f0 · ytui 0.3.17\n\
                     restart ytui to use the new binary\n";
        assert_eq!(
            marked_line(out, '✔').as_deref(),
            Some("✔ updated to 108d1f0 · ytui 0.3.17")
        );
        assert_eq!(marked_line(out, '✗'), None);
        assert_eq!(
            marked_line("✗ build failed, the installed binary was left alone", '✗').as_deref(),
            Some("✗ build failed, the installed binary was left alone")
        );
    }

    #[test]
    fn tokenless_scan_result_wins() {
        let found = vec![
            remote::Found {
                url: "http://10.0.0.2:8787".into(),
                version: "0.4.0".into(),
                auth: true,
                ms: 1,
                solve_ms: None,
            },
            remote::Found {
                url: "http://10.0.0.3:8787".into(),
                version: "0.4.0".into(),
                auth: false,
                ms: 50,
                solve_ms: None,
            },
        ];
        let chosen = pick_resolver(&ScanState::Done(found), &Config::default()).unwrap();
        assert_eq!(chosen.url, "http://10.0.0.3:8787");
    }

    #[test]
    fn token_protected_results_are_ignored_without_a_token() {
        let found = vec![remote::Found {
            url: "http://10.0.0.2:8787".into(),
            version: "0.4.0".into(),
            auth: true,
            ms: 1,
            solve_ms: None,
        }];
        assert!(pick_resolver(&ScanState::Done(found), &Config::default()).is_none());
    }

    #[test]
    fn token_protected_results_are_used_when_a_token_exists() {
        let cfg = Config {
            resolver_token: "secret".into(),
            ..Config::default()
        };
        let found = vec![remote::Found {
            url: "http://10.0.0.2:8787".into(),
            version: "0.4.0".into(),
            auth: true,
            ms: 1,
            solve_ms: None,
        }];
        assert!(pick_resolver(&ScanState::Done(found), &cfg).is_some());
    }

    #[test]
    fn update_labels_and_animation_agree() {
        assert_eq!(
            UpdateState::Available("0.3.19".into()).label(),
            "0.3.19 available"
        );
        assert_eq!(UpdateState::Current.label(), "up to date");
        assert!(UpdateState::Checking.animating());
        assert!(UpdateState::Updating.animating());
        assert!(!UpdateState::Idle.animating());
        assert!(!UpdateState::Available("0.3.19".into()).animating());
    }
}
