//! Drawing. The layout follows Spotify's: a sidebar of playlists, the song
//! list, and the player along the bottom, with a vim-style status line under it.

use crate::app::{App, Confirm, Focus, Mode, Popup, View};
use crate::editor;
use crate::theme::*;
use crate::wave;
use vimj_core::compat::{Compat, TempoRelation};
use vimj_core::protocol::*;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use unicode_width::UnicodeWidthStr;

const SIDEBAR_WIDTH: u16 = 28;

pub fn draw(frame: &mut Frame, app: &mut App) {
    let screen = frame.area();
    frame.render_widget(Block::new().style(Style::new().bg(Color::Black)), screen);
    let [top, player, status] = Layout::vertical([Constraint::Min(5), Constraint::Length(4), Constraint::Length(1)]).areas(screen);
    if app.editor.is_some() {
        draw_editor(frame, top, app);
    } else {
        let [side, main] = Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(20)]).areas(top);
        draw_sidebar(frame, side, app);
        match app.view {
            View::Downloads => draw_downloads(frame, main, app),
            View::Playlist(_) => draw_list(frame, main, app),
        }
    }
    draw_player(frame, player, app);
    draw_status(frame, status, app);
    if let Some(popup) = &app.popup {
        match popup {
            Popup::Help => draw_help(frame, screen),
            Popup::Suggestions(_) => draw_suggestions(frame, screen, app),
            Popup::AddTo(_) => draw_add_to(frame, screen, app),
        }
    }
}

fn panel(title: &str, focused: bool) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { GREEN } else { FAINT }))
        .title(Span::styled(format!(" {title} "), if focused { bold(text()) } else { subtle() }))
        .style(Style::new().bg(PANEL))
}

/// Clears an area for a popup. A wide character straddling its left edge
/// would otherwise eat the border, so blank that too.
pub fn clear(frame: &mut Frame, area: Rect) {
    frame.render_widget(Clear, area);
    if area.x == 0 {
        return;
    }
    let buf = frame.buffer_mut();
    for y in area.y..area.y + area.height {
        let cell = &mut buf[(area.x - 1, y)];
        if cell.symbol().width() > 1 {
            cell.set_char(' ');
        }
    }
}

fn scroll_to(offset: usize, selected: usize, height: usize, margin: usize) -> usize {
    if height == 0 {
        return 0;
    }
    let margin = margin.min(height.saturating_sub(1) / 2);
    if selected < offset + margin {
        selected.saturating_sub(margin)
    } else if selected + margin >= offset + height {
        selected + margin + 1 - height
    } else {
        offset
    }
}

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == Focus::Sidebar;
    let block = panel("vimj", focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;
    let playing_context = app.player.as_ref().filter(|p| p.status != PlayerStatus::Stopped).and_then(|p| p.context.clone());
    let active_downloads = app.downloads.iter().filter(|d| !matches!(d.state, DownloadState::Done | DownloadState::Failed)).count();
    let mut lines = Vec::new();
    for (i, (view, name)) in app.side_items().into_iter().enumerate() {
        if i == 2 {
            lines.push(Line::raw(""));
            lines.push(Line::styled(" PLAYLISTS", bold(dim())));
        }
        let count = match &view {
            View::Downloads if active_downloads > 0 => format!("{active_downloads}↓"),
            View::Downloads => String::new(),
            View::Playlist(id) if id == LIBRARY_ID => app.playlists.as_ref().map_or(String::new(), |p| p.library.count.to_string()),
            View::Playlist(id) => app.playlists.as_ref().and_then(|p| p.playlists.iter().find(|x| x.id == *id)).map_or(String::new(), |p| p.count.to_string()),
        };
        let current = app.view == view;
        let playing = matches!(&view, View::Playlist(id) if playing_context.as_deref() == Some(id));
        let marker = if playing { "♪ " } else if current { "▸ " } else { "  " };
        let label = fit(&format!("{marker}{name}"), width.saturating_sub(count.width() + 2));
        let mut style = if current { bold(text()) } else { subtle() };
        if playing {
            style = style.fg(GREEN);
        }
        let mut line = Line::from(vec![Span::styled(format!(" {label}"), style), Span::styled(format!("{count} "), dim())]);
        if focused && i == app.side {
            line = line.style(Style::new().bg(HIGHLIGHT));
        }
        lines.push(line);
    }
    if app.playlists.as_ref().is_some_and(|p| p.playlists.is_empty()) {
        lines.push(Line::styled("   :new <name> makes one", dim()));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

struct Columns {
    title: usize,
    artist: usize,
    link: usize,
}

impl Columns {
    /// marker 2, number 5, bpm 6, key 4, energy 2, time 6, spacing
    const FIXED: usize = 27;

    fn new(width: u16, mix: bool) -> Columns {
        let width = width as usize;
        let link = if mix && width > 90 { 26 } else { 0 };
        let flexible = width.saturating_sub(Columns::FIXED + link);
        let title = flexible * 55 / 100;
        Columns { title, artist: flexible - title, link }
    }
}

fn draw_list(frame: &mut Frame, area: Rect, app: &mut App) {
    let focused = app.focus == Focus::Main;
    let Some(list) = app.list.as_ref().filter(|l| Some(l.id.as_str()) == app.current_id()) else {
        frame.render_widget(panel("Library", focused), area);
        let inner = area.inner(ratatui::layout::Margin::new(2, 1));
        let message = if app.connected { "Loading…" } else { "Connecting to vimjd…" };
        frame.render_widget(Paragraph::new(Line::styled(message, dim())), inner);
        return;
    };
    let block = panel(&list.name, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [header, columns_row, rows_area] = Layout::vertical([Constraint::Length(2), Constraint::Length(1), Constraint::Min(1)]).areas(inner);

    // Header: counts, mix state, hints.
    let total: u64 = list.songs.iter().map(|s| s.song.duration_ms).sum();
    let analysing = list.songs.iter().filter(|s| s.state == SongState::Analyzing).count();
    let mut summary = vec![
        Span::styled(format!(" {} · {}", plural(list.songs.len(), "song"), long_duration(total)), subtle()),
        Span::styled(if list.mix { "  ·  Mix on" } else { "  ·  Mix off" }, if list.mix { accent() } else { dim() }),
    ];
    if analysing > 0 {
        summary.push(Span::styled(format!("  ·  analysing {analysing}"), Style::new().fg(WARN)));
    }
    let hints = if list.songs.is_empty() && list.id == LIBRARY_ID {
        " No songs yet. Paste a YouTube link, or :add <link>. Files in the music folder are picked up too."
    } else if list.songs.is_empty() {
        " Empty. Yank songs in the library (yy, or V to select) and put them here with p, or press a on them."
    } else {
        " t next-song ideas · e edit transition · s smart reorder · m mix · V select · ? help"
    };
    frame.render_widget(Paragraph::new(vec![Line::from(summary), Line::styled(hints, dim())]), header);

    let cols = Columns::new(inner.width, list.mix);
    let head = format!(
        "  {:>3}  {}{}{:>5} {:>4} E  {:>5}  {}",
        "#",
        fit("TITLE", cols.title),
        fit("ARTIST", cols.artist),
        "BPM",
        "KEY",
        "TIME",
        if cols.link > 0 { "INTO NEXT" } else { "" }
    );
    frame.render_widget(Paragraph::new(Line::styled(head, bold(dim()))), columns_row);

    let height = rows_area.height as usize;
    app.page = height;
    app.list_area = rows_area;
    app.offset = scroll_to(app.offset, app.cursor, height, 3);
    let playing = app.player.as_ref().filter(|p| p.status != PlayerStatus::Stopped && p.context.as_deref() == Some(&list.id));
    let playing_index = playing.and_then(|p| p.index);
    let mixing_into = playing.and_then(|p| p.mixing.as_ref()).and(playing_index.map(|i| i + 1));
    let (sel_from, sel_to) = match app.visual {
        Some(anchor) => (anchor.min(app.cursor), anchor.max(app.cursor)),
        None => (usize::MAX, 0),
    };
    let search = app.search.as_ref().map(|s| s.to_lowercase());
    let mut lines = Vec::new();
    for (i, info) in list.songs.iter().enumerate().skip(app.offset).take(height) {
        let song = &info.song;
        let a = song.analysis.as_ref();
        let is_playing = playing_index == Some(i);
        let marker = if is_playing {
            Span::styled("▶ ", accent())
        } else if mixing_into == Some(i) {
            Span::styled("◆ ", Style::new().fg(PURPLE))
        } else {
            Span::raw("  ")
        };
        let title_style = match info.state {
            SongState::Missing => Style::new().fg(ERROR),
            _ if is_playing => bold(accent()),
            _ => text(),
        };
        let matched = search.as_ref().is_some_and(|q| song.title.to_lowercase().contains(q) || song.artist.to_lowercase().contains(q));
        let title_style = if matched { title_style.add_modifier(Modifier::UNDERLINED) } else { title_style };
        let bpm = match (info.state, a.and_then(|a| a.bpm())) {
            (SongState::Analyzing, _) => Span::styled(format!("{:>5}", "…"), Style::new().fg(WARN)),
            (SongState::Missing, _) => Span::styled(format!("{:>5}", "gone"), Style::new().fg(ERROR)),
            (_, Some(bpm)) => Span::styled(format!("{bpm:>5.0}"), subtle()),
            (_, None) => Span::styled(format!("{:>5}", "—"), dim()),
        };
        let key = match camelot(a) {
            Some(c) => Span::styled(format!(" {:>4}", c.to_string()), Style::new().fg(camelot_color(c))),
            None => Span::styled(format!(" {:>4}", "—"), dim()),
        };
        let energy = match a {
            Some(a) => Span::styled(format!(" {} ", energy_glyph(a.energy)), Style::new().fg(energy_color(a.energy))),
            None => Span::styled("   ", dim()),
        };
        let mut spans = vec![
            marker,
            Span::styled(format!("{:>3}  ", i + 1), dim()),
            Span::styled(fit(&song.title, cols.title), title_style),
            Span::styled(fit(&song.artist, cols.artist), subtle()),
            bpm,
            key,
            energy,
            Span::styled(format!(" {:>5}  ", clock(song.duration_ms as f64)), dim()),
        ];
        if cols.link > 0
            && let Some(link) = list.links.get(i)
        {
            let t = &link.transition;
            let (glyph, color) = quality(link.compat.score);
            spans.push(Span::styled(format!("{glyph} "), Style::new().fg(color)));
            let label = format!("{} {}b", t.preset.label(), t.bars);
            spans.push(Span::styled(fit(&label, cols.link - 7), if t.auto { dim() } else { Style::new().fg(PURPLE) }));
            spans.push(Span::styled(format!("{:>3.0}%", link.compat.score * 100.0), Style::new().fg(color)));
        }
        let mut line = Line::from(spans);
        if (sel_from..=sel_to).contains(&i) {
            line = line.style(Style::new().bg(SELECTED));
        }
        if i == app.cursor {
            line = line.style(Style::new().bg(if focused { HIGHLIGHT } else { Color::Rgb(28, 28, 28) }));
            if (sel_from..=sel_to).contains(&i) {
                line = line.style(Style::new().bg(Color::Rgb(50, 90, 66)));
            }
        }
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(lines), rows_area);
}

/// A dot whose colour says how well two songs flow.
fn quality(score: f32) -> (char, Color) {
    let color = if score >= 0.8 {
        GREEN
    } else if score >= 0.6 {
        Color::Rgb(170, 220, 90)
    } else if score >= 0.4 {
        WARN
    } else {
        ERROR
    };
    ('●', color)
}

/// "92% · same key · +1.2% tempo · energy +0.1"
pub fn compat_spans(c: &Compat) -> Vec<Span<'static>> {
    let (_, color) = quality(c.score);
    let mut spans = vec![Span::styled(format!("{:.0}% match", c.score * 100.0), bold(Style::new().fg(color)))];
    if let Some(k) = c.key {
        spans.push(Span::styled(format!(" · {} → {} {}", k.from, k.to, k.relation.label()), subtle()));
    }
    if let Some(t) = c.tempo {
        let fold = match t.relation {
            TempoRelation::Direct => "",
            TempoRelation::Double => " (double time)",
            TempoRelation::Half => " (half time)",
        };
        spans.push(Span::styled(format!(" · tempo {:+.1}%{fold}", t.diff * 100.0), subtle()));
    }
    if let Some(e) = c.energy {
        spans.push(Span::styled(format!(" · energy {:+.2}", e.delta()), subtle()));
    }
    if c.key.is_none() && c.tempo.is_none() {
        spans.push(Span::styled(" · not analysed yet", dim()));
    }
    spans
}

fn draw_downloads(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = panel("Downloads", app.focus == Focus::Main);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;
    let mut lines = vec![
        Line::styled(" Paste a link (YouTube, YouTube Music, SoundCloud, Bandcamp…) or :add <link>. Playlists work too.", dim()),
        Line::styled(" Audio is kept as the site serves it, without re-encoding. dd cancels or clears · :clear tidies up.", dim()),
    ];
    if let Some(status) = &app.status {
        let tool = match &status.yt_dlp {
            Some(v) => Span::styled(format!(" yt-dlp {v}"), dim()),
            None => Span::styled(" yt-dlp isn't installed or doesn't run, so downloads can't work", Style::new().fg(ERROR)),
        };
        let cookies = match &status.cookies_from_browser {
            Some(b) => format!(" · using {b}'s YouTube login"),
            None => " · :cookies <browser> if YouTube asks you to sign in".into(),
        };
        lines.push(Line::from(vec![tool, Span::styled(cookies, dim())]));
    }
    lines.push(Line::raw(""));
    if app.downloads.is_empty() {
        lines.push(Line::styled("   Nothing downloading.", dim()));
    }
    for (i, d) in app.downloads.iter().enumerate() {
        let (icon, color, state) = match d.state {
            DownloadState::Queued => ("·", DIM, "queued".to_owned()),
            DownloadState::Resolving => ("…", WARN, "looking up".to_owned()),
            DownloadState::Downloading => ("↓", BLUE, format!("{:>3.0}%", d.progress * 100.0)),
            DownloadState::Analyzing => ("◌", PURPLE, "analysing".to_owned()),
            DownloadState::Done => ("✓", GREEN, "done".to_owned()),
            DownloadState::Failed => ("✗", ERROR, "failed".to_owned()),
        };
        let name = d.title.clone().unwrap_or_else(|| d.url.clone());
        let bar_width = 20;
        let bar = if d.state == DownloadState::Downloading {
            let filled = (d.progress * bar_width as f32).round() as usize;
            format!(" {}{} ", "━".repeat(filled), "─".repeat(bar_width - filled))
        } else {
            " ".repeat(bar_width + 2)
        };
        let speed = d.speed.map_or(String::new(), |s| format!("{:.1} MB/s", s / 1_000_000.0));
        let name_width = width.saturating_sub(bar_width + 30);
        let mut line = Line::from(vec![
            Span::styled(format!(" {icon} "), Style::new().fg(color)),
            Span::styled(fit(&name, name_width), text()),
            Span::styled(bar, Style::new().fg(BLUE)),
            Span::styled(format!("{state:<10}"), Style::new().fg(color)),
            Span::styled(speed, dim()),
        ]);
        if i == app.download_cursor && app.focus == Focus::Main {
            line = line.style(Style::new().bg(HIGHLIGHT));
        }
        lines.push(line);
        if let Some(e) = &d.error {
            let style = if d.state == DownloadState::Failed { Style::new().fg(ERROR) } else { dim() };
            lines.push(Line::styled(format!("     {}", fit(e, width.saturating_sub(6))), style));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_player(frame: &mut Frame, area: Rect, app: &mut App) {
    let block = Block::new().borders(Borders::TOP).border_style(Style::new().fg(FAINT)).style(Style::new().bg(PANEL));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [info, wave_row, next_row] = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Length(1)]).areas(inner);
    let Some(p) = app.player.clone().filter(|p| p.song.is_some()) else {
        let message = if app.connected { "  Nothing playing. Enter plays the song under the cursor." } else { "  Not connected to vimjd." };
        frame.render_widget(Paragraph::new(Line::styled(message, dim())), info);
        return;
    };
    let song = p.song.as_ref().expect("filtered");
    let position = app.position_ms().min(song.duration_ms as f64);
    let icon = match p.status {
        PlayerStatus::Playing => "▶",
        PlayerStatus::Paused => "⏸",
        PlayerStatus::Loading => "…",
        PlayerStatus::Stopped => "■",
    };
    let a = song.analysis.as_ref();
    let mut spans = vec![
        Span::styled(format!("  {icon} "), accent()),
        Span::styled(song.title.clone(), bold(text())),
        Span::styled(if song.artist.is_empty() { String::new() } else { format!(" — {}", song.artist) }, subtle()),
    ];
    if let Some(bpm) = a.and_then(|a| a.bpm()) {
        spans.push(Span::styled(format!("   {:.1} BPM", bpm * p.rate), subtle()));
        if (p.rate - 1.0).abs() > 0.0005 {
            spans.push(Span::styled(format!(" ({:+.1}%)", (p.rate - 1.0) * 100.0), Style::new().fg(PURPLE)));
        }
    }
    if let Some(c) = camelot(a) {
        spans.push(Span::styled(format!("  {c}"), Style::new().fg(camelot_color(c))));
    }
    if p.preview {
        spans.push(Span::styled("   previewing a transition", Style::new().fg(PURPLE)));
    }
    let volume = format!("vol {:>3}%  ", p.volume);
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    spans.push(Span::raw(" ".repeat((info.width as usize).saturating_sub(used + volume.len()))));
    spans.push(Span::styled(volume, dim()));
    frame.render_widget(Paragraph::new(Line::from(spans)), info);

    // Waveform overview with progress and the outgoing window.
    let width = wave_row.width.saturating_sub(16) as usize;
    let duration = song.duration_ms as f64;
    let wave = app.waveforms.get(&song.id).cloned();
    let window = p.transition.as_ref().map(|t| {
        let tm = t.timing(a.and_then(|a| a.grid).as_ref(), p.next.as_ref().and_then(|n| n.analysis.as_ref()).and_then(|a| a.grid).as_ref());
        (tm.out_start_ms, tm.out_end_ms)
    });
    let mut spans = vec![Span::styled(format!("  {:>5} ", clock(position)), subtle())];
    for c in 0..width {
        let (from, to) = (duration * c as f64 / width as f64, duration * (c + 1) as f64 / width as f64);
        let played = to <= position;
        let in_window = window.is_some_and(|(s, e)| to > s && from < e);
        let level = wave.as_ref().and_then(|w| wave::column(w, from, to)).map_or(0.1, |col| col.peak);
        let glyph = BLOCKS[1 + (level * 7.0).round() as usize].to_string();
        let color = match (played, in_window) {
            (true, _) => GREEN,
            (false, true) => PURPLE,
            (false, false) => DIM,
        };
        let style = if from <= position && position < to { Style::new().fg(TEXT).bg(Color::Rgb(60, 60, 60)) } else { Style::new().fg(color) };
        spans.push(Span::styled(glyph, style));
    }
    spans.push(Span::styled(format!(" {:<5}", clock(duration)), dim()));
    frame.render_widget(Paragraph::new(Line::from(spans)), wave_row);

    // What comes next.
    let next_line = if let Some(m) = &p.mixing {
        let filled = (m.progress * 20.0).round() as usize;
        Line::from(vec![
            Span::styled("  ◆ mixing into ", Style::new().fg(PURPLE)),
            Span::styled(m.song.title.clone(), bold(text())),
            Span::styled(format!("  {}{}", "▰".repeat(filled), "▱".repeat(20 - filled)), Style::new().fg(PURPLE)),
            Span::styled(p.transition.as_ref().map_or(String::new(), |t| format!("  {}", transition_summary(t))), dim()),
        ])
    } else if let Some(next) = &p.next {
        let when = window.map(|(start, _)| start - position).filter(|ms| *ms > 0.0);
        let mut spans = vec![Span::styled("  next  ", dim()), Span::styled(next.title.clone(), subtle())];
        match (&p.transition, when) {
            (Some(t), Some(ms)) => spans.push(Span::styled(format!("  · {} in {}", transition_summary(t), clock(ms)), dim())),
            (Some(t), None) => spans.push(Span::styled(format!("  · {}", transition_summary(t)), dim())),
            (None, _) => spans.push(Span::styled("  · back to back", dim())),
        }
        Line::from(spans)
    } else {
        Line::styled("  last song", dim())
    };
    frame.render_widget(Paragraph::new(next_line), next_row);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let (label, color) = match (&app.mode, app.visual.is_some(), app.editor.is_some()) {
        (Mode::Command(_), _, _) => ("COMMAND", BLUE),
        (Mode::Search { .. }, _, _) => ("SEARCH", BLUE),
        (Mode::Confirm(_), _, _) => ("CONFIRM", ERROR),
        (_, _, true) => ("EDIT", PURPLE),
        (_, true, _) => ("VISUAL", PURPLE),
        _ => ("NORMAL", GREEN),
    };
    let mut spans = vec![Span::styled(format!(" {label} "), bold(Style::new().fg(Color::Black).bg(color))), Span::raw(" ")];
    let mut cursor_x = None;
    match &app.mode {
        Mode::Command(line) => {
            spans.push(Span::styled(format!(":{}", line.text), text()));
            cursor_x = Some(label.len() as u16 + 4 + line.text[..line.cursor].width() as u16);
        }
        Mode::Search { input, .. } => {
            spans.push(Span::styled(format!("/{}", input.text), text()));
            cursor_x = Some(label.len() as u16 + 4 + input.text[..input.cursor].width() as u16);
        }
        Mode::Confirm(c) => {
            let question = match c {
                Confirm::DeleteSongs(songs) if songs.len() == 1 => format!("Delete \"{}\" and its file? y/n", songs[0].1),
                Confirm::DeleteSongs(songs) => format!("Delete {} and their files? y/n", plural(songs.len(), "song")),
                Confirm::DeletePlaylist(_, name) => format!("Delete the playlist \"{name}\"? Its songs stay in the library. y/n"),
            };
            spans.push(Span::styled(question, bold(Style::new().fg(ERROR))));
        }
        Mode::Normal => {
            if !app.pending.is_empty() {
                spans.push(Span::styled(format!("{}  ", app.pending), accent()));
            }
            if let Some((level, message, _)) = &app.toast {
                let color = match level {
                    Level::Info => SUBTLE,
                    Level::Warn => WARN,
                    Level::Error => ERROR,
                };
                spans.push(Span::styled(message.clone(), Style::new().fg(color)));
            }
        }
    }
    let right = match &app.view {
        View::Playlist(_) if app.songs_len() > 0 => format!(" {}/{} ", app.cursor + 1, app.songs_len()),
        _ => String::new(),
    };
    let used: usize = spans.iter().map(|s| s.content.width()).sum();
    spans.push(Span::raw(" ".repeat((area.width as usize).saturating_sub(used + right.len()))));
    spans.push(Span::styled(right, dim()));
    frame.render_widget(Paragraph::new(Line::from(spans)).style(Style::new().bg(Color::Rgb(24, 24, 24))), area);
    if let Some(x) = cursor_x {
        frame.set_cursor_position((area.x + x.min(area.width.saturating_sub(1)), area.y));
    }
}

fn draw_editor(frame: &mut Frame, area: Rect, app: &App) {
    let Some(ed) = &app.editor else { return };
    let block = panel("Transition", true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    editor::draw(frame, inner, ed, &app.waveforms, app.player.as_ref());
}

fn centered(screen: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(screen.width.saturating_sub(4));
    let height = height.min(screen.height.saturating_sub(2));
    Rect { x: screen.x + (screen.width - width) / 2, y: screen.y + (screen.height - height) / 2, width, height }
}

fn modal(title: &str) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(GREEN))
        .title(Span::styled(format!(" {title} "), bold(text())))
        .style(Style::new().bg(Color::Rgb(28, 28, 28)))
}

fn draw_suggestions(frame: &mut Frame, screen: Rect, app: &App) {
    let Some(Popup::Suggestions(p)) = &app.popup else { return };
    let area = centered(screen, 96, 18);
    clear(frame, area);
    let block = modal(&format!("Best next songs after {}", p.anchor_title));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let width = inner.width as usize;
    let mut lines = vec![Line::styled(" Picked from your whole library by key, tempo and energy. Enter moves it in after.", dim()), Line::raw("")];
    match &p.items {
        None => lines.push(Line::styled(" Looking…", dim())),
        Some(items) if items.is_empty() => lines.push(Line::styled(" No other songs yet.", dim())),
        Some(items) => {
            for (i, s) in items.iter().enumerate() {
                let song = &s.song.song;
                let a = song.analysis.as_ref();
                let (glyph, color) = quality(s.compat.score);
                let where_ = match s.index {
                    Some(at) if at == p.anchor + 1 => "already next".to_owned(),
                    Some(at) => format!("now #{}", at + 1),
                    None => "not in this list".to_owned(),
                };
                let mut line = Line::from(vec![
                    Span::styled(format!(" {} ", i + 1), dim()),
                    Span::styled(format!("{glyph} {:>3.0}% ", s.compat.score * 100.0), Style::new().fg(color)),
                    Span::styled(fit(&song.title, width.saturating_sub(58).max(10)), bold(text())),
                    Span::styled(fit(&format!(" {}", song.artist), 18), subtle()),
                    Span::styled(a.and_then(|a| a.bpm()).map_or("    —".into(), |b| format!("{b:>5.0}")), subtle()),
                    match camelot(a) {
                        Some(c) => Span::styled(format!(" {:>4}", c.to_string()), Style::new().fg(camelot_color(c))),
                        None => Span::styled("    —", dim()),
                    },
                    Span::styled(format!("  {where_}"), dim()),
                ]);
                if i == p.cursor {
                    line = line.style(Style::new().bg(HIGHLIGHT));
                }
                lines.push(line);
                if i == p.cursor {
                    let mut detail = vec![Span::raw("        ")];
                    detail.extend(compat_spans(&s.compat).into_iter().skip(1));
                    lines.push(Line::from(detail));
                }
            }
        }
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(" j/k choose · Enter or 1-5 put it next · Esc close", dim()));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_add_to(frame: &mut Frame, screen: Rect, app: &App) {
    let Some(Popup::AddTo(p)) = &app.popup else { return };
    let targets = app.add_targets();
    let area = centered(screen, 46, targets.len() as u16 + 5);
    clear(frame, area);
    let block = modal(&format!("Add {} to", plural(p.songs.len(), "song")));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = Vec::new();
    for (i, (id, name)) in targets.iter().enumerate() {
        let style = if id.is_none() { accent() } else { text() };
        let mut line = Line::styled(format!(" {name}"), style);
        if i == p.cursor {
            line = line.style(Style::new().bg(HIGHLIGHT));
        }
        lines.push(line);
    }
    lines.push(Line::raw(""));
    lines.push(Line::styled(" j/k choose · Enter add · Esc close", dim()));
    frame.render_widget(Paragraph::new(lines), inner);
}

const HELP: [(&str, &[(&str, &str)]); 5] = [
    ("Move", &[
        ("j k  ↓ ↑", "down, up (with a count: 5j)"),
        ("gg G  12G", "top, bottom, line 12"),
        ("C-d C-u C-f C-b", "half page, page"),
        ("h l  Tab", "sidebar, list"),
        ("/ n N", "search titles and artists"),
        ("gl gd gc", "library, downloads, what's playing"),
    ]),
    ("Edit", &[
        ("dd x  3dd", "remove (library: delete file)"),
        ("yy  p P", "yank, put after / before"),
        ("J K", "move down / up"),
        ("V", "select lines, then d y J K a"),
        ("a", "add to a playlist"),
        ("u  C-r", "undo, redo"),
    ]),
    ("Mix", &[
        ("t", "five best next songs, from the library"),
        ("e", "edit the transition after this song"),
        ("s", "smart reorder"),
        ("m", "mix on or off"),
    ]),
    ("Play", &[
        ("Enter  space", "play from here, pause"),
        ("> <", "next, previous song"),
        ("] [  } {", "seek 10 s, 30 s"),
        ("+ -", "volume"),
    ]),
    ("Commands", &[
        (":add <link>", "download (or paste a link)"),
        (":new :rename :delete", "playlists"),
        (":title :artist", "rename the song"),
        (":cookies <browser>", "use its YouTube login"),
        (":rescan :clear", "pick up files, tidy downloads"),
        ("q  Q", "quit, quit and stop music"),
    ]),
];

fn draw_help(frame: &mut Frame, screen: Rect) {
    let column = |sections: &[(&str, &[(&str, &str)])]| -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for (section, keys) in sections {
            lines.push(Line::styled(format!(" {section}"), bold(accent())));
            for (key, what) in keys.iter() {
                lines.push(Line::from(vec![Span::styled(format!("   {key:<23}"), text()), Span::styled((*what).to_owned(), subtle())]));
            }
            lines.push(Line::raw(""));
        }
        lines
    };
    let (left, right) = (column(&HELP[..2]), column(&HELP[2..]));
    let rows = left.len().max(right.len()) as u16 + 4;
    let area = centered(screen, 124, rows);
    clear(frame, area);
    let block = modal("Keys");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [body, foot] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(inner);
    let [l, r] = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).areas(body);
    frame.render_widget(Paragraph::new(left), l);
    frame.render_widget(Paragraph::new(right), r);
    frame.render_widget(
        Paragraph::new(Line::styled(" Transition editor: j/k field · h/l change · space preview loop · [ ] other pairs · esc close", dim())),
        foot,
    );
}
