//! ratatui drawing. Catppuccin-ish palette that fits Omarchy/Hyprland setups.

use crate::app::{App, PlayerBtn, TABS, TAB_LIKED, TAB_PLAYLISTS, TAB_QUEUE, TAB_SEARCH, TAB_SUBS};
use crate::audio::{Repeat, Status};
use crate::entry::{fmt_duration, fmt_f64};
use crate::ytdl;
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::prelude::{Color, Line, Modifier, Span, Style};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};
use ratatui::Frame;
use std::time::Duration;

const ACCENT: Color = Color::Rgb(137, 220, 235);
const GREEN: Color = Color::Rgb(166, 227, 161);
const YELLOW: Color = Color::Rgb(249, 226, 175);
const RED: Color = Color::Rgb(243, 139, 168);
const DIM: Color = Color::Rgb(110, 115, 140);
const FG: Color = Color::Rgb(205, 214, 244);
const BORDER: Color = Color::Rgb(88, 96, 120);
const SELBG: Color = Color::Rgb(49, 54, 73);

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let rows = Layout::vertical([
        Constraint::Length(1), // tabs + account
        Constraint::Length(4), // player
        Constraint::Min(4),    // list
        Constraint::Length(1), // status / toast
        Constraint::Length(1), // hints
    ])
    .split(area);
    draw_tabs(f, app, rows[0]);
    draw_player(f, app, rows[1]);
    draw_list(f, app, rows[2]);
    draw_status(f, app, rows[3]);
    draw_hints(f, app, rows[4]);
    if app.search_open {
        draw_search(f, app, area);
    }
    if app.help_open {
        draw_help(f, app, area);
    }
}

fn draw_tabs(f: &mut Frame, app: &mut App, area: Rect) {
    app.tab_hits.clear();
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    let mut x: u16 = 1;
    for (i, name) in TABS.iter().enumerate() {
        let n = app.tab_count(i);
        let label = if n > 0 {
            format!(" {} {}·{} ", i + 1, name, n)
        } else {
            format!(" {} {} ", i + 1, name)
        };
        let style = if i == app.tab {
            Style::new()
                .fg(Color::Rgb(30, 32, 48))
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(DIM)
        };
        let w = label.chars().count() as u16;
        spans.push(Span::styled(label, style));
        app.tab_hits.push((x, x + w - 1));
        x += w + 1;
        spans.push(Span::raw(" "));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);

    let (text, color) = match &app.account {
        Some(name) => (format!("◉ {name} "), GREEN),
        None => ("○ not connected ".to_string(), DIM),
    };
    let w = text.chars().count() as u16;
    if area.width > w + x + 1 {
        let r = Rect {
            x: area.x + area.width - w,
            y: area.y,
            width: w,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(text, Style::new().fg(color))),
            r,
        );
    }
}

fn draw_player(f: &mut Frame, app: &mut App, area: Rect) {
    app.player_buttons.clear();
    let (status, title, channel, pos, dur, vol, muted, shuffle, repeat) = {
        let st = app.player.lock().unwrap_or_else(|e| e.into_inner());
        (
            st.status,
            st.title.clone(),
            st.channel.clone(),
            st.pos,
            st.dur,
            st.volume,
            st.muted,
            st.shuffle,
            st.repeat,
        )
    };

    let block = Block::bordered()
        .border_style(Style::new().fg(BORDER))
        .title(Span::styled(" player ", Style::new().fg(DIM)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let w = inner.width as usize;
    if w < 10 {
        app.gauge_area = Rect::default();
        return;
    }

    if matches!(status, Status::Stopped | Status::Error) {
        app.gauge_area = Rect::default();
        let first = if status == Status::Error {
            let err = app
                .player
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .err
                .clone()
                .unwrap_or_default();
            Line::from(Span::styled(
                format!(" ✗ {}", truncate(&err, w.saturating_sub(4))),
                Style::new().fg(RED),
            ))
        } else {
            Line::from(Span::styled(
                " stopped — / to search · enter plays the whole list · ? for help",
                Style::new().fg(DIM),
            ))
        };
        let second = if app.cfg.has_cookies() {
            Line::from(Span::styled(
                format!(" ♪ 48 kHz stereo · volume {vol}%"),
                Style::new().fg(DIM),
            ))
        } else {
            Line::from(Span::styled(
                " not connected — run: ytui auth <cookies.txt> for liked music, playlists, subscriptions",
                Style::new().fg(YELLOW),
            ))
        };
        f.render_widget(Paragraph::new(vec![first, second]), inner);
        return;
    }

    // The glyph is the *action*: ⏸ while playing, ⏵ while paused. Use the
    // same media family as ⏮/⏭ so all three render at one size; ◌ comes from
    // a different font and looked tiny next to them.
    let (icon, icol) = match status {
        Status::Paused => ("⏵", YELLOW),
        Status::Loading => ("⟳", ACCENT),
        _ => ("⏸", GREEN),
    };

    let mut flags = String::new();
    if shuffle {
        flags.push_str("⤨ ");
    }
    match repeat {
        Repeat::All => flags.push_str("↻ "),
        Repeat::One => flags.push_str("↻1 "),
        Repeat::Off => {}
    }
    let vol_txt = if muted {
        "muted".to_string()
    } else {
        format!("{vol}%")
    };
    let right = format!("{flags}{vol_txt}");

    let shown = if title.is_empty() {
        "…".to_string()
    } else {
        title
    };

    // Clickable transport: " ⏮  ▶  ⏭  " (10 cells), then the title.
    const PREFIX: usize = 10;
    let start = inner.x + 1;
    let buttons = [
        (PlayerBtn::Prev, start.saturating_sub(1)),
        (PlayerBtn::PlayPause, start + 2),
        (PlayerBtn::Next, start + 5),
    ];
    for (btn, x) in buttons {
        app.player_buttons.push((
            Rect {
                x,
                y: inner.y,
                width: 3,
                height: 1,
            },
            btn,
        ));
    }
    let btn_style = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);

    let right_w = right.chars().count();
    let title_w = w.saturating_sub(right_w + PREFIX).max(4);
    let t = truncate(&shown, title_w);
    let pad = " ".repeat(w.saturating_sub(PREFIX + t.chars().count() + right_w));
    let line1 = Line::from(vec![
        Span::raw(" "),
        Span::styled("⏮", btn_style),
        Span::raw("  "),
        Span::styled(icon, Style::new().fg(icol).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled("⏭", btn_style),
        Span::raw("  "),
        Span::styled(t, Style::new().fg(FG).add_modifier(Modifier::BOLD)),
        Span::raw(pad),
        Span::styled(right, Style::new().fg(DIM)),
    ]);

    let time_str = format!("{} / {}", fmt_f64(pos), fmt_f64(dur));
    let gw = w.saturating_sub(time_str.chars().count() + 3).max(1);
    let frac = if dur > 0.0 {
        (pos / dur).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = ((frac * gw as f64) as usize).min(gw);
    let head = if filled < gw { "●" } else { "" };
    let rest = "─".repeat(gw.saturating_sub(filled + head.len()));
    let mut spans = vec![
        Span::raw(" "),
        Span::styled("━".repeat(filled), Style::new().fg(ACCENT)),
        Span::styled(head, Style::new().fg(GREEN)),
        Span::styled(rest, Style::new().fg(DIM)),
        Span::styled(format!(" {time_str}"), Style::new().fg(DIM)),
    ];
    if !channel.is_empty() {
        let room = w.saturating_sub(gw + time_str.chars().count() + 6);
        if room > 6 {
            spans.push(Span::styled(
                format!("  {}", truncate(&channel, room)),
                Style::new().fg(DIM),
            ));
        }
    }
    app.gauge_area = Rect {
        x: inner.x + 1,
        y: inner.y + 1,
        width: gw as u16,
        height: 1,
    };

    f.render_widget(Paragraph::new(vec![line1, Line::from(spans)]), inner);
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let title = if app.tab == TAB_PLAYLISTS && app.in_playlist {
        format!(" Playlists · {} · esc back ", app.playlist_title)
    } else {
        format!(" {} ", TABS[app.tab])
    };
    let block = Block::bordered()
        .border_style(Style::new().fg(BORDER))
        .title(Span::styled(&title, Style::new().fg(ACCENT)));
    let inner = block.inner(area);
    app.list_items_area = inner;
    f.render_widget(block, area);

    let selidx = app.sel[app.tab];

    if app.tab == TAB_QUEUE {
        if app.queue_rows.is_empty() {
            empty_hint(
                f,
                app,
                inner,
                "queue is empty — press enter on a list to play it, a to append",
            );
            return;
        }
        let items: Vec<ListItem> = app
            .queue_rows
            .iter()
            .map(|(title, current)| {
                let (marker, mstyle) = if *current {
                    ("▶ ", Style::new().fg(GREEN))
                } else {
                    ("  ", Style::new().fg(DIM))
                };
                ListItem::new(Line::from(vec![
                    Span::styled(marker, mstyle),
                    Span::styled(title.clone(), Style::new().fg(FG)),
                ]))
            })
            .collect();
        render_items(f, app, items, selidx, inner);
        return;
    }

    let list = app.current_vec();
    if list.is_empty() {
        let hint = match app.tab {
            TAB_SEARCH => "press / and search YouTube Music (or paste a URL)",
            TAB_LIKED => "no liked music yet — press r (needs ytui auth)",
            TAB_PLAYLISTS => {
                if app.in_playlist {
                    "empty playlist"
                } else {
                    "no playlists yet — press r (needs ytui auth)"
                }
            }
            TAB_SUBS => "no subscriptions loaded — press r (needs ytui auth)",
            _ => "nothing played yet",
        };
        empty_hint(f, app, inner, hint);
        return;
    }

    let is_playlist_list = app.tab == TAB_PLAYLISTS && !app.in_playlist;
    let items: Vec<ListItem> = list
        .iter()
        .map(|e| {
            let right = if is_playlist_list {
                format!("  {} playlists", e.channel)
            } else {
                format!("  {} · {}", e.channel, e.duration_str())
            };
            ListItem::new(Line::from(vec![
                Span::styled(e.title.clone(), Style::new().fg(FG)),
                Span::styled(right, Style::new().fg(DIM)),
            ]))
        })
        .collect();
    render_items(f, app, items, selidx, inner);
}

fn render_items(f: &mut Frame, app: &mut App, items: Vec<ListItem>, selidx: usize, inner: Rect) {
    let mut st = std::mem::take(&mut app.list_state);
    st.select(Some(selidx.min(items.len() - 1)));
    let list = List::new(items).highlight_style(
        Style::new()
            .bg(SELBG)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    f.render_stateful_widget(list, inner, &mut st);
    app.list_offset = st.offset();
    app.list_state = st;
}

fn empty_hint(f: &mut Frame, _app: &mut App, inner: Rect, hint: &str) {
    f.render_widget(
        Paragraph::new(Span::styled(hint, Style::new().fg(DIM))).alignment(Alignment::Center),
        inner,
    );
}

fn draw_status(f: &mut Frame, app: &mut App, area: Rect) {
    let loading = app.loading.map(|(k, t)| {
        let frames = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let i = (t.elapsed().as_millis() / 80) as usize % 10;
        (frames[i], ytdl::label(k))
    });
    let toast = app
        .toast
        .as_ref()
        .filter(|(_, t, _)| t.elapsed() < Duration::from_secs(5))
        .map(|(m, _, e)| (m.clone(), *e));

    let line = if let Some((spin, what)) = loading {
        Line::from(Span::styled(
            format!(" {spin} fetching {what}…"),
            Style::new().fg(ACCENT),
        ))
    } else if let Some((msg, err)) = toast {
        Line::from(Span::styled(
            format!(" {msg}"),
            Style::new().fg(if err { RED } else { GREEN }),
        ))
    } else {
        // Read everything out of the state lock before calling anything that
        // might take it again (tab_count locks for the queue tab).
        let dur = {
            let st = app.player.lock().unwrap_or_else(|e| e.into_inner());
            fmt_duration(st.dur.max(0.0) as u64)
        };
        let count = app.tab_count(app.tab);
        Line::from(Span::styled(
            format!(" ytui · {} · {} tracks · {}", TABS[app.tab], count, dur),
            Style::new().fg(DIM),
        ))
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_hints(f: &mut Frame, _app: &App, area: Rect) {
    let hints = " 1-6 tabs · / search · enter play list · a queue · space pause · ←→/h l seek · , . ±30s · [ ] vol · <> track · s shuffle · R repeat · m mute · x stop · r refresh · ? help · q quit";
    let s: String = hints.chars().take(area.width as usize).collect();
    f.render_widget(Paragraph::new(Span::styled(s, Style::new().fg(DIM))), area);
}

fn draw_search(f: &mut Frame, app: &mut App, area: Rect) {
    let w = (area.width * 6 / 10).max(40).min(area.width);
    let r = centered_rect(w, 3, area);
    f.render_widget(Clear, r);
    let b = Block::bordered()
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(
            " search / paste URL or playlist ",
            Style::new().fg(ACCENT),
        ));
    let inner = b.inner(r);
    f.render_widget(b, r);
    f.render_widget(
        Paragraph::new(Span::styled(app.search_input.clone(), Style::new().fg(FG))),
        inner,
    );
    let cx = inner.x + app.search_input.chars().count() as u16;
    if cx < inner.x + inner.width {
        f.set_cursor_position(Position::new(cx, inner.y));
    }
}

fn draw_help(f: &mut Frame, _app: &mut App, area: Rect) {
    let (w, h) = (68u16, 28u16);
    let r = centered_rect(w.min(area.width), h.min(area.height), area);
    f.render_widget(Clear, r);
    let b = Block::bordered()
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(
            " ytui help — press any key to close ",
            Style::new().fg(ACCENT),
        ));
    let inner = b.inner(r);
    f.render_widget(b, r);

    let rows: [(&str, &str); 25] = [
        ("— navigation —", ""),
        ("1-6 / Tab / Shift+Tab", "switch tabs"),
        ("↑↓ / j k / wheel", "move selection"),
        ("click tab", "switch to that tab"),
        ("click / double-click", "select / play item"),
        ("enter", "play the whole list from here"),
        ("a", "append selection to queue"),
        ("esc", "leave a playlist drill-down"),
        ("— playback —", ""),
        ("space or p", "pause / resume"),
        ("← → or h l", "seek ∓/±5s"),
        (", .", "seek ∓/±30s"),
        ("click progress bar", "seek to that position"),
        ("click ⏮ ⏸ ⏭", "previous · pause/resume · next"),
        ("[ ]", "volume ∓/±5"),
        ("m", "mute / unmute"),
        ("s", "shuffle"),
        ("R", "repeat off → all → one"),
        ("< >", "previous / next track"),
        ("x", "stop playback"),
        ("— library —", ""),
        ("d", "delete from history / queue"),
        ("c", "copy link to clipboard"),
        ("r", "refresh list · / search"),
        ("q", "quit"),
    ];
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, d)| {
            if d.is_empty() {
                Line::from(Span::styled(
                    format!(" {k}"),
                    Style::new().fg(DIM).add_modifier(Modifier::BOLD),
                ))
            } else {
                Line::from(vec![
                    Span::styled(format!("  {k:<22}"), Style::new().fg(ACCENT)),
                    Span::styled(*d, Style::new().fg(FG)),
                ])
            }
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}
