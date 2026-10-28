//! Application state, key bindings and mouse handling.

use crate::audio::{self, Cmd, Repeat, State as PlayerState, Status, Track};
use crate::auth;
use crate::config::{self, Config};
use crate::entry::{fmt_f64, Entry};
use crate::ui;
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
use std::sync::mpsc::{Receiver, Sender};
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

/// Clickable transport control in the player bar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerBtn {
    Prev,
    PlayPause,
    Next,
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
    pub toast: Option<(String, Instant, bool)>,
    pub search_open: bool,
    pub search_input: String,
    pub help_open: bool,
    pub last_query: String,
    pub should_quit: bool,
    pub player: Arc<Mutex<PlayerState>>,
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
            toast: None,
            search_open: false,
            search_input: String::new(),
            help_open: false,
            last_query: String::new(),
            should_quit: false,
            player: Arc::clone(&player.state),
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
            terminal.draw(|f| ui::draw(f, app))?;
            if poll(Duration::from_millis(120))? {
                match read()? {
                    Event::Key(k) => {
                        if k.kind != KeyEventKind::Release {
                            app.on_key(k);
                        }
                    }
                    Event::Mouse(m) => app.on_mouse(m),
                    _ => {}
                }
            }
            while let Ok(m) = fetch_rx.try_recv() {
                app.on_fetch(m);
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
                        .map(|(i, t)| (t.title.clone(), i == st.index))
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
            Msg::Begin(kind) => {
                self.loading = Some((kind, Instant::now()));
                match kind {
                    Kind::PlaylistTracks => {
                        self.playlist_tracks.clear();
                        self.in_playlist = true;
                        self.playlist_title = self.pending_playlist_title.clone();
                        self.sel[TAB_PLAYLISTS] = 0;
                    }
                    _ => {
                        self.list_mut(kind).clear();
                        self.sel[self.tab_of(kind)] = 0;
                    }
                }
            }
            Msg::Add(kind, entry) => {
                if kind == Kind::PlaylistTracks {
                    self.playlist_tracks.push(entry);
                } else {
                    self.list_mut(kind).push(entry);
                }
            }
            Msg::Done(kind, label) => {
                self.loading = None;
                self.clamp_sel();
                let _ = kind;
                self.set_toast(format!("✔ {label}"), false);
            }
            Msg::Error(kind, err) => {
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

    /// The engine cannot reach the toast directly, so it bumps a counter when
    /// it walks past a track YouTube would not serve.
    fn pick_up_notice(&mut self) {
        let (seq, msg) = {
            let st = lock(&self.player);
            (st.notice_seq, st.notice.clone())
        };
        if seq != self.notice_seq {
            self.notice_seq = seq;
            if let Some(msg) = msg {
                self.set_toast(msg, true);
            }
        }
    }

    // ---------------- keys ----------------

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(k.code, KeyCode::Char('c') | KeyCode::Char('d')) {
                self.should_quit = true;
            }
            return;
        }
        if self.search_open {
            self.on_key_search(k);
            return;
        }
        if self.help_open {
            self.help_open = false;
            return;
        }

        match k.code {
            KeyCode::Char('q') | KeyCode::Char('Q') => self.should_quit = true,
            KeyCode::Char('?') => self.help_open = true,
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
                            self.set_toast("double-click (or enter) to play", false);
                        }
                    }
                }
            }
            MouseEventKind::ScrollUp => self.move_sel(-1),
            MouseEventKind::ScrollDown => self.move_sel(1),
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
            TAB_LIKED if self.liked.is_empty() => self.fetch_liked(false),
            TAB_PLAYLISTS if self.playlists.is_empty() => self.fetch_playlists(false),
            TAB_SUBS if self.subs.is_empty() => self.fetch_subs(false),
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
        self.loading = Some((kind, Instant::now()));
        ytdl::spawn(kind, target, self.cfg.clone(), self.fetch_tx.clone());
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
            self.set_toast("already in the queue, press enter to play it", false);
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

fn which(bin: &str) -> bool {
    if let Ok(paths) = std::env::var("PATH") {
        for dir in paths.split(':') {
            if std::path::Path::new(dir).join(bin).is_file() {
                return true;
            }
        }
    }
    false
}
