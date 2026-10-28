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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const ACCENT: Color = Color::Rgb(137, 220, 235);
const GREEN: Color = Color::Rgb(166, 227, 161);
const YELLOW: Color = Color::Rgb(249, 226, 175);
const RED: Color = Color::Rgb(243, 139, 168);
const DIM: Color = Color::Rgb(110, 115, 140);
const FG: Color = Color::Rgb(205, 214, 244);
const BORDER: Color = Color::Rgb(88, 96, 120);
const SELBG: Color = Color::Rgb(49, 54, 73);

/// Braille wheel used for every background wait, in the status line and in
/// place of the play button while a track is being resolved.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn spinner(elapsed: Duration) -> char {
    SPINNER[(elapsed.as_millis() / 80) as usize % SPINNER.len()]
}

/// Transport buttons. The media codepoints (⏮ U+23EE, ⏸ U+23F8, ⏭ U+23ED)
/// are emoji-presentation characters: terminals hand them to a colour emoji
/// font, which draws tiny coloured blobs at the baseline, and most monospace
/// fonts do not carry them at all. These glyphs are plain text characters, so
/// they always come out monochrome at the cell size, whatever the font.
const ICON_PREV: &str = "◀◀";
const ICON_NEXT: &str = "▶▶";
const ICON_PAUSE: &str = "‖";
const ICON_PLAY: &str = "▶";
/// Blank cells between two transport buttons.
const GAP: usize = 2;

/// Cells taken by " ◀◀  ▶  ▶▶  ": the leading space, the three buttons and
/// their gaps. The title, its padding and the click areas are placed after it.
fn transport_width(icon: &str) -> usize {
    1 + ICON_PREV.width() + GAP + icon.width() + GAP + ICON_NEXT.width() + GAP
}

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
    app.vol_area = Rect::default();
    app.mute_area = Rect::default();
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
                " stopped. / to search, enter plays the whole list, ? for help",
                Style::new().fg(DIM),
            ))
        };
        let second = if app.cfg.has_cookies() {
            Line::from(Span::styled(
                format!(" ♪ stereo · volume {vol}%"),
                Style::new().fg(DIM),
            ))
        } else {
            Line::from(Span::styled(
                " not connected. Run ytui auth <cookies.txt> for liked music, playlists and subscriptions",
                Style::new().fg(YELLOW),
            ))
        };
        f.render_widget(Paragraph::new(vec![first, second]), inner);
        return;
    }

    // The glyph is the *action*: pause while playing, play while paused. While
    // yt-dlp is working out the stream URL the button becomes the same wheel as
    // the status line, so a requested track visibly spins until it starts.
    let (icon, icol) = match status {
        Status::Paused => (ICON_PLAY.to_string(), YELLOW),
        Status::Loading => (spinner(app.anim.elapsed()).to_string(), ACCENT),
        _ => (ICON_PAUSE.to_string(), GREEN),
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
    let pct = if muted {
        "muted".to_string()
    } else {
        format!("{vol}%")
    };
    // Fixed width so the slider does not move while the number changes.
    // Fixed width so the number and the bar keep their columns; right-aligned
    // because the value sits directly left of the slider.
    let pct = format!("{pct:>5}");

    let shown = if title.is_empty() {
        "…".to_string()
    } else {
        title
    };

    // Clickable transport: " ◀◀  ‖  ▶▶  ", then the title. The hit boxes cover
    // the glyph plus the gap that follows it, so the one-cell pause button is
    // as easy to hit as the two-cell skip buttons.
    let prefix = transport_width(&icon);
    let start = inner.x + 1;
    let play_x = start + (ICON_PREV.width() + GAP) as u16;
    let next_x = play_x + (icon.width() + GAP) as u16;
    let buttons = [
        (PlayerBtn::Prev, start, ICON_PREV.width() as u16 + 1),
        (PlayerBtn::PlayPause, play_x, icon.width() as u16 + GAP as u16),
        (PlayerBtn::Next, next_x, ICON_NEXT.width() as u16 + 1),
    ];
    for (btn, x, width) in buttons {
        app.player_buttons.push((
            Rect {
                x,
                y: inner.y,
                width,
                height: 1,
            },
            btn,
        ));
    }
    let btn_style = Style::new().fg(ACCENT).add_modifier(Modifier::BOLD);

    // Volume block on the right: percentage, 10 slider cells, mute icon. The
    // icon comes last on purpose: a font is free to draw the speaker emoji one
    // cell wide or two, and anything drawn after it would land a cell off,
    // taking the click areas with it.
    const VOL_W: usize = 10;
    const ICON_W: usize = 2;
    let speaker = if muted { "🔇" } else { "🔊" };
    let flags_w = flags.width();
    let right_w = flags_w + pct.width() + 1 + VOL_W + 1 + ICON_W + 1;
    let right_start = inner.x + (w.saturating_sub(right_w)) as u16;
    app.vol_area = Rect {
        x: right_start + (flags_w + pct.width() + 1) as u16,
        y: inner.y,
        width: VOL_W as u16,
        height: 1,
    };
    app.mute_area = Rect {
        x: right_start + (right_w - ICON_W - 1) as u16,
        y: inner.y,
        width: ICON_W as u16,
        height: 1,
    };
    let filled = ((vol.clamp(0, 100) as f64 / 100.0) * (VOL_W - 1) as f64).round() as usize;
    let filled = filled.min(VOL_W - 1);
    let (bar_fg, head_fg) = if muted { (DIM, DIM) } else { (ACCENT, GREEN) };
    let bar_filled = "━".repeat(filled);
    let bar_rest = "─".repeat(VOL_W - filled - 1);

    let gap = " ".repeat(GAP);
    let title_w = w.saturating_sub(right_w + prefix).max(4);
    let t = truncate(&shown, title_w);
    let pad = " ".repeat(w.saturating_sub(prefix + t.width() + right_w));
    let line1 = Line::from(vec![
        Span::raw(" "),
        Span::styled(ICON_PREV, btn_style),
        Span::raw(gap.clone()),
        Span::styled(icon, Style::new().fg(icol).add_modifier(Modifier::BOLD)),
        Span::raw(gap.clone()),
        Span::styled(ICON_NEXT, btn_style),
        Span::raw(gap),
        Span::styled(t, Style::new().fg(FG).add_modifier(Modifier::BOLD)),
        Span::raw(pad),
        Span::styled(flags, Style::new().fg(DIM)),
        Span::styled(pct, Style::new().fg(DIM)),
        Span::raw(" "),
        Span::styled(bar_filled, Style::new().fg(bar_fg)),
        Span::styled("●", Style::new().fg(head_fg)),
        Span::styled(bar_rest, Style::new().fg(DIM)),
        Span::raw(" "),
        Span::styled(speaker, Style::new()),
        Span::raw(" "),
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
                "queue is empty. Press enter on a list to play it, a to append",
            );
            return;
        }
        let len = app.queue_rows.len();
        let (start, end) = window(app.list_offset, selidx, inner.height as usize, len);
        let items: Vec<ListItem> = app.queue_rows[start..end]
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
        app.list_offset = start;
        render_items(f, app, items, selidx - start, inner);
        return;
    }

    let list = app.current_vec();
    if list.is_empty() {
        let hint = if app.loading.is_some() {
            "loading…"
        } else {
            match app.tab {
                TAB_SEARCH => "press / and search YouTube Music (or paste a URL)",
                TAB_LIKED => "no liked music yet, press r (needs ytui auth)",
                TAB_PLAYLISTS => {
                    if app.in_playlist {
                        "empty playlist"
                    } else {
                        "no playlists yet, press r (needs ytui auth)"
                    }
                }
                TAB_SUBS => "no subscriptions loaded, press r (needs ytui auth)",
                _ => "nothing played yet",
            }
        };
        empty_hint(f, app, inner, hint);
        return;
    }

    let playing = {
        let st = app.player.lock().unwrap_or_else(|e| e.into_inner());
        st.url.clone()
    };

    let is_playlist_list = app.tab == TAB_PLAYLISTS && !app.in_playlist;
    // Video id of the playing track, so the marker can be decided without
    // formatting a URL for every row on screen.
    let playing_id = playing
        .split_once("v=")
        .map(|(_, rest)| rest.split('&').next().unwrap_or(rest))
        .unwrap_or("");
    let len = list.len();
    let (start, end) = window(app.list_offset, selidx, inner.height as usize, len);
    let items: Vec<ListItem> = list[start..end]
        .iter()
        .map(|e| {
            let now = !is_playlist_list
                && !playing.is_empty()
                && if e.url_override.is_empty() {
                    e.id == playing_id
                } else {
                    e.url_override == playing
                };
            let (marker, mstyle) = if now {
                ("▶ ", Style::new().fg(GREEN))
            } else {
                ("  ", Style::new().fg(DIM))
            };
            let right = if is_playlist_list {
                format!("  {} playlists", e.channel)
            } else {
                format!("  {} · {}", e.channel, e.duration_str())
            };
            let title_style = if now {
                Style::new().fg(GREEN).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(FG)
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker, mstyle),
                Span::styled(e.title.clone(), title_style),
                Span::styled(right, Style::new().fg(DIM)),
            ]))
        })
        .collect();
    app.list_offset = start;
    render_items(f, app, items, selidx - start, inner);
}

/// First visible row for a viewport of `height` rows, keeping `sel` on screen.
/// `offset` is the previous frame's window start, so a one-row move scrolls by
/// one row instead of re-centring the list.
fn window(offset: usize, sel: usize, height: usize, len: usize) -> (usize, usize) {
    if len == 0 {
        return (0, 0);
    }
    let height = height.max(1);
    let last_start = len.saturating_sub(height);
    let sel = sel.min(len - 1);
    let mut start = offset.min(last_start);
    if sel < start {
        start = sel;
    } else if sel >= start + height {
        start = sel + 1 - height;
    }
    start = start.min(last_start);
    (start, (start + height).min(len))
}

fn render_items(f: &mut Frame, app: &mut App, items: Vec<ListItem>, selidx: usize, inner: Rect) {
    let mut st = std::mem::take(&mut app.list_state);
    st.select(Some(selidx.min(items.len().saturating_sub(1))));
    // The list only sees the visible slice, so its own offset is always zero;
    // the absolute window start lives in `app.list_offset`.
    *st.offset_mut() = 0;
    let list = List::new(items).highlight_style(
        Style::new()
            .bg(SELBG)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    f.render_stateful_widget(list, inner, &mut st);
    // The window start is absolute; the list widget only knows the slice.
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
        (spinner(t.elapsed()), ytdl::label(k), app.fetch_count)
    });
    let toast = app
        .toast
        .as_ref()
        .filter(|(_, t, _)| t.elapsed() < Duration::from_secs(5))
        .map(|(m, _, e)| (m.clone(), *e));

    let line = if let Some((spin, what, rows)) = loading {
        let detail = if rows > 0 {
            format!(" {spin} fetching {what}… {rows}")
        } else {
            format!(" {spin} fetching {what}…")
        };
        Line::from(Span::styled(detail, Style::new().fg(ACCENT)))
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
    let hints = " 1-6 tabs · / search · enter play list · a queue · space pause · ←→/h l seek · , . 30s · [ ] vol · <> track · s shuffle · R repeat · m mute · x stop · r refresh · ? help · q quit";
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
            " ytui help, press any key to close ",
            Style::new().fg(ACCENT),
        ));
    let inner = b.inner(r);
    f.render_widget(b, r);

    let rows: [(&str, &str); 26] = [
        ("navigation", ""),
        ("1-6 / Tab / Shift+Tab", "switch tabs"),
        ("↑↓ / j k / wheel", "move selection"),
        ("click tab", "switch to that tab"),
        ("click / double-click", "select / play item"),
        ("enter", "play the whole list from here"),
        ("a", "append selection to queue"),
        ("esc", "leave a playlist drill-down"),
        ("playback", ""),
        ("space or p", "pause / resume"),
        ("← → or h l", "seek 5s back/forward"),
        (", .", "seek 30s back/forward"),
        ("click progress bar", "seek to that position"),
        ("click ◀◀ ‖ ▶▶", "previous, pause/resume, next"),
        ("drag volume slider", "set the volume"),
        ("[ ]", "volume down/up by 5"),
        ("m / click icon", "mute / unmute"),
        ("s", "shuffle"),
        ("R", "repeat off / all / one"),
        ("< >", "previous / next track"),
        ("x", "stop playback"),
        ("library", ""),
        ("d", "delete from history / queue"),
        ("c", "copy link to clipboard"),
        ("r", "refresh list, / search"),
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
    if s.width() <= max {
        return s.to_string();
    }
    if max < 2 {
        return "…".repeat(max);
    }
    // Leave one cell for the ellipsis and measure in cells rather than chars,
    // so a wide glyph in a title does not push the rest of the line along.
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(1);
        if used + w + 1 > max {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The player line right-aligns the volume block by padding after the
    /// title. If the padding is computed in chars while the title contains
    /// wide glyphs, the block slides left of its mouse area.
    #[test]
    fn wide_titles_fit_their_column() {
        let prefix = transport_width(ICON_PAUSE);
        const RIGHT_W: usize = 20; // " 100%" + " " + 10 bar cells + " " + icon + " "
        let w = 100;
        let title_w = w - RIGHT_W - prefix;
        for title in [
            "plain ascii title",
            "Căpitănescu - Choke Me",
            "漢字のタイトルです",
            "👨‍👩‍👧 family",
            "🇷🇴 Romania",
        ] {
            let t = truncate(title, title_w);
            assert!(
                t.width() <= title_w,
                "{title:?} became {t:?}, {} cells for {title_w}",
                t.width()
            );
        }
    }

    /// The media codepoints (⏮/⏸/⏭) are emoji-presentation: terminals hand
    /// them to a colour emoji font, which draws tiny blobs, and most monospace
    /// fonts do not have them at all. The transport glyphs must stay plain text
    /// and one or two cells wide, or the click areas drift from the drawing.
    #[test]
    fn transport_glyphs_are_text_width() {
        for glyph in [ICON_PREV, ICON_NEXT, ICON_PAUSE, ICON_PLAY] {
            assert_eq!(
                glyph.width(),
                glyph.chars().count(),
                "{glyph:?} must be one cell per character"
            );
            assert!(glyph.width() <= 2, "{glyph:?} is too wide");
        }
        let loading = spinner(Duration::ZERO).to_string();
        for icon in [ICON_PAUSE, ICON_PLAY, loading.as_str()] {
            assert_eq!(transport_width(icon), 12, "{icon:?} moves the title");
        }
    }

    /// A windowed list only builds the rows on screen, but the selection must
    /// still be able to move past the viewport at either end.
    #[test]
    fn window_keeps_the_selection_visible() {
        assert_eq!(window(0, 0, 20, 500), (0, 20));
        assert_eq!(window(0, 19, 20, 500), (0, 20));
        assert_eq!(window(0, 20, 20, 500), (1, 21));
        assert_eq!(window(400, 399, 20, 500), (399, 419));
        assert_eq!(window(480, 499, 20, 500), (480, 500));
        assert_eq!(window(0, 0, 20, 5), (0, 5));
        assert_eq!(window(0, 0, 0, 0), (0, 0));
    }
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
