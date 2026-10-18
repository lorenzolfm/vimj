//! The transition editor: both songs' waveforms lined up on one timeline,
//! the volume, EQ and effect curves under them, and a field list edited the
//! vim way: `j`/`k` pick a field, `h`/`l` change it.

use crate::theme::{self, *};
use crate::wave;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use vimj_core::model::{BEATS_PER_BAR, Grid};
use vimj_core::protocol::{PlayerState, TransitionDetail};
use vimj_core::transition::{BAR_CHOICES, DeckFx, Effect, Eq, Preset, Transition, Volume};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Edits are sent to the daemon this long after the last key press.
const SAVE_AFTER: Duration = Duration::from_millis(300);
/// Fine nudge for lining beats up by ear.
const NUDGE_MS: f64 = 10.0;
/// The outgoing song plays this long before the window when previewing.
pub const LEAD_IN_MS: f64 = 4_000.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Preset,
    Length,
    Sync,
    Out,
    In,
    Volume,
    Eq,
    Effect,
}

const FIELDS: [Field; 8] = [Field::Preset, Field::Length, Field::Sync, Field::Out, Field::In, Field::Volume, Field::Eq, Field::Effect];

pub struct Editor {
    /// The pair is `songs[index]` → `songs[index + 1]`.
    pub index: usize,
    pub count: usize,
    pub detail: Option<TransitionDetail>,
    pub draft: Option<Transition>,
    pub field: Field,
    dirty_since: Option<Instant>,
    /// The last edit moved the window, so a running preview should start over.
    pub restart_preview: bool,
    pub previewing: bool,
}

pub enum Action {
    None,
    Close,
    Reset,
    TogglePreview,
    /// Move to the pair this many places away.
    Pair(isize),
}

impl Editor {
    pub fn new(index: usize, count: usize) -> Editor {
        Editor { index, count, detail: None, draft: None, field: Field::Preset, dirty_since: None, restart_preview: false, previewing: false }
    }

    pub fn loaded(&mut self, detail: TransitionDetail) {
        // Keep local edits that haven't been saved yet.
        if self.dirty_since.is_none() {
            self.draft = Some(detail.transition.clone());
        }
        self.detail = Some(detail);
    }

    pub fn pair(&self) -> Option<(String, String)> {
        self.detail.as_ref().map(|d| (d.from.song.id.clone(), d.to.song.id.clone()))
    }

    /// The draft to save, once edits have settled.
    pub fn due(&mut self) -> Option<Transition> {
        let since = self.dirty_since?;
        if since.elapsed() < SAVE_AFTER {
            return None;
        }
        self.take_dirty()
    }

    pub fn take_dirty(&mut self) -> Option<Transition> {
        self.dirty_since.take()?;
        self.draft.clone()
    }

    fn grids(&self) -> (Option<Grid>, Option<Grid>) {
        let grid = |s: &vimj_core::protocol::SongInfo| s.song.analysis.as_ref().and_then(|a| a.grid);
        self.detail.as_ref().map_or((None, None), |d| (grid(&d.from), grid(&d.to)))
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => return Action::Close,
            KeyCode::Char(' ') => return Action::TogglePreview,
            KeyCode::Char('[') => return Action::Pair(-1),
            KeyCode::Char(']') => return Action::Pair(1),
            KeyCode::Char('R') | KeyCode::Char('0') => return Action::Reset,
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => self.step_field(1),
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => self.step_field(-1),
            KeyCode::Char('g') => self.field = FIELDS[0],
            KeyCode::Char('G') => self.field = FIELDS[FIELDS.len() - 1],
            KeyCode::Char(c @ '1'..='6') => {
                let preset = Preset::ALL[c as usize - '1' as usize];
                self.edit(true, |t| t.set_preset(preset));
            }
            KeyCode::Char('s') => self.edit(true, |t| t.sync = !t.sync),
            KeyCode::Char('+') | KeyCode::Char('=') => self.change(Field::Length, 1, false),
            KeyCode::Char('-') => self.change(Field::Length, -1, false),
            KeyCode::Char('h') | KeyCode::Left => self.change(self.field, -1, shift),
            KeyCode::Char('l') | KeyCode::Right => self.change(self.field, 1, shift),
            KeyCode::Char('H') => self.change(self.field, -1, true),
            KeyCode::Char('L') => self.change(self.field, 1, true),
            KeyCode::Char('b') => self.change(self.field, -(PHRASE_STEPS as i32), true),
            KeyCode::Char('w') => self.change(self.field, PHRASE_STEPS as i32, true),
            KeyCode::Char(',') => self.nudge(-NUDGE_MS),
            KeyCode::Char('.') => self.nudge(NUDGE_MS),
            _ => {}
        }
        Action::None
    }

    fn step_field(&mut self, by: isize) {
        let at = FIELDS.iter().position(|f| *f == self.field).unwrap_or(0) as isize;
        self.field = FIELDS[(at + by).rem_euclid(FIELDS.len() as isize) as usize];
    }

    fn edit(&mut self, moves_window: bool, f: impl FnOnce(&mut Transition)) {
        let Some(t) = self.draft.as_mut() else { return };
        f(t);
        t.auto = false;
        self.dirty_since = Some(Instant::now());
        self.restart_preview |= moves_window;
    }

    /// `coarse` moves cue points by a bar instead of a beat.
    fn change(&mut self, field: Field, by: i32, coarse: bool) {
        let (a, b) = self.grids();
        let beat = |g: Option<Grid>| g.map_or(500.0, |g| g.beat_ms());
        let step = |g: Option<Grid>| beat(g) * if coarse { f64::from(BEATS_PER_BAR) } else { 1.0 } * f64::from(by);
        match field {
            Field::Preset => self.edit(true, |t| {
                let at = Preset::ALL.iter().position(|p| *p == t.preset).map_or(0, |i| i as i32 + by);
                t.set_preset(Preset::ALL[at.rem_euclid(Preset::ALL.len() as i32) as usize]);
            }),
            Field::Length => self.edit(true, |t| {
                let at = BAR_CHOICES.iter().position(|b| *b >= t.bars).unwrap_or(BAR_CHOICES.len() - 1) as i32;
                t.bars = BAR_CHOICES[(at + by).clamp(0, BAR_CHOICES.len() as i32 - 1) as usize];
            }),
            Field::Sync => self.edit(true, |t| t.sync = !t.sync),
            Field::Out => self.edit(true, |t| t.out_ms = (t.out_ms + step(a)).max(0.0)),
            Field::In => self.edit(true, |t| t.in_ms = (t.in_ms + step(b)).max(0.0)),
            Field::Volume => self.edit(false, |t| {
                t.volume = cycle(&Volume::ALL, t.volume, by);
                t.customize();
            }),
            Field::Eq => self.edit(false, |t| {
                t.eq = cycle(&Eq::ALL, t.eq, by);
                t.customize();
            }),
            Field::Effect => self.edit(false, |t| {
                t.effect = cycle(&Effect::ALL, t.effect, by);
                t.customize();
            }),
        }
    }

    fn nudge(&mut self, ms: f64) {
        match self.field {
            Field::Out => self.edit(true, |t| t.out_ms = (t.out_ms + ms).max(0.0)),
            _ => self.edit(true, |t| t.in_ms = (t.in_ms + ms).max(0.0)),
        }
    }
}

/// `w`/`b` step a phrase of eight bars.
const PHRASE_STEPS: u32 = 8;

fn cycle<T: Copy + PartialEq>(all: &[T], current: T, by: i32) -> T {
    let at = all.iter().position(|x| *x == current).unwrap_or(0) as i32;
    all[(at + by).rem_euclid(all.len() as i32) as usize]
}

/// Draws the editor over `area`.
pub fn draw(frame: &mut Frame, area: Rect, editor: &Editor, waveforms: &HashMap<String, Arc<Vec<u8>>>, player: Option<&PlayerState>) {
    let (Some(detail), Some(t)) = (&editor.detail, &editor.draft) else {
        frame.render_widget(Paragraph::new(Line::styled("  Loading transition…", dim())), area);
        return;
    };
    let width = area.width.saturating_sub(4) as usize;
    let (ga, gb) = (detail.from.song.analysis.as_ref().and_then(|a| a.grid), detail.to.song.analysis.as_ref().and_then(|a| a.grid));
    let timing = t.timing(ga.as_ref(), gb.as_ref());
    let mut lines: Vec<Line> = Vec::new();

    // Header: the pair, and how well they match.
    let song_label = |s: &vimj_core::protocol::SongInfo| {
        let a = s.song.analysis.as_ref();
        let mut spans = vec![Span::styled(s.song.title.clone(), bold(text()))];
        if !s.song.artist.is_empty() {
            spans.push(Span::styled(format!(" — {}", s.song.artist), subtle()));
        }
        if let Some(bpm) = a.and_then(|a| a.bpm()) {
            spans.push(Span::styled(format!("  {bpm:.1} BPM"), subtle()));
        }
        if let Some(c) = theme::camelot(a) {
            spans.push(Span::styled(format!("  {c}"), Style::new().fg(camelot_color(c))));
        }
        spans
    };
    lines.push(Line::from(vec![
        Span::styled(format!(" Transition {}/{}", editor.index + 1, editor.count.saturating_sub(1)), bold(accent())),
        Span::styled(if detail.saved || !t.auto { "   edited" } else { "   automatic" }, dim()),
        Span::styled(if editor.previewing { "   ▶ previewing" } else { "" }, accent()),
    ]));
    let mut pair = vec![Span::raw(" A  ")];
    pair.extend(song_label(&detail.from));
    lines.push(Line::from(pair));
    let mut pair = vec![Span::raw(" B  ")];
    pair.extend(song_label(&detail.to));
    lines.push(Line::from(pair));
    let mut matching = vec![Span::styled("    ", dim())];
    matching.extend(crate::ui::compat_spans(&detail.compat));
    lines.push(Line::from(matching));
    lines.push(Line::raw(""));

    // Timeline: the window plus half its length either side.
    let pad = (timing.duration_ms * 0.5).max(4_000.0);
    let (t0, t1) = (-pad, timing.duration_ms + pad);
    let ms_per_col = (t1 - t0) / width.max(1) as f64;
    let col_time = |c: usize| t0 + c as f64 * ms_per_col;
    let playhead = player.filter(|p| p.preview && editor.previewing).and_then(|p| preview_time(p, t, &timing));
    let head_col = playhead.map(|ms| ((ms - t0) / ms_per_col) as isize);

    let lane = |wave: Option<&Arc<Vec<u8>>>, position: &dyn Fn(f64) -> f64, lit: &dyn Fn(f64) -> bool, height: usize| -> Vec<Line<'static>> {
        let mut rows: Vec<Vec<Span>> = vec![Vec::new(); height];
        for c in 0..width {
            let (from, to) = (position(col_time(c)), position(col_time(c + 1)));
            let column = wave.and_then(|w| wave::column(w, from, to));
            let on = lit(col_time(c));
            let level = column.map_or(0.0, |col| col.peak);
            let color = column.map_or(FAINT, |col| wave::color(col, on));
            let eighths = (level * (height * 8) as f32).round() as usize;
            let at_head = head_col == Some(c as isize);
            for (r, row) in rows.iter_mut().enumerate() {
                // Rows run top to bottom; fill from the bottom.
                let from_bottom = height - 1 - r;
                let fill = eighths.saturating_sub(from_bottom * 8).min(8);
                let glyph = if at_head { '│' } else { BLOCKS[fill] };
                let style = if at_head { Style::new().fg(TEXT) } else { Style::new().fg(color) };
                row.push(Span::styled(glyph.to_string(), style));
            }
        }
        rows.into_iter().map(|mut r| {
            r.insert(0, Span::raw("  "));
            Line::from(r)
        }).collect()
    };
    let ruler = |grid: Option<Grid>, position: &dyn Fn(f64) -> f64, lit: &dyn Fn(f64) -> bool| -> Line<'static> {
        let mut spans = vec![Span::raw("  ")];
        for c in 0..width {
            let (from, to) = (position(col_time(c)), position(col_time(c + 1)));
            let glyph = match grid {
                Some(_) if to <= 0.0 => ' ',
                Some(g) => {
                    let first = g.beat_at(from).ceil();
                    if g.beat_time(first) < to {
                        if first.rem_euclid(4.0) == 0.0 { '┃' } else { '╵' }
                    } else {
                        ' '
                    }
                }
                None => ' ',
            };
            let on = lit(col_time(c));
            let color = match glyph {
                '┃' if on => GREEN,
                '┃' => SUBTLE,
                _ => DIM,
            };
            spans.push(Span::styled(glyph.to_string(), Style::new().fg(color)));
        }
        Line::from(spans)
    };

    let a_pos = |ms: f64| t.out_ms + ms;
    let b_pos = |ms: f64| t.in_ms + ms * timing.rate;
    let a_lit = |ms: f64| ms < timing.duration_ms;
    let b_lit = |ms: f64| ms >= 0.0;
    let selected = |f: Field| editor.field == f;
    let lane_label = |name: &str, f: Field| {
        Line::from(Span::styled(format!(" {name}"), if selected(f) { bold(accent()) } else { subtle() }))
    };
    lines.push(lane_label(&format!("A leaves at {} (beat {})", clock_tenths(t.out_ms), beat_label(ga, t.out_ms)), Field::Out));
    lines.extend(lane(waveforms.get(&detail.from.song.id), &a_pos, &a_lit, 3));
    lines.push(ruler(ga, &a_pos, &a_lit));
    lines.push(lane_label(&format!("B enters at {} (beat {})", clock_tenths(t.in_ms), beat_label(gb, t.in_ms)), Field::In));
    lines.extend(lane(waveforms.get(&detail.to.song.id), &b_pos, &b_lit, 3));
    lines.push(ruler(gb, &b_pos, &b_lit));

    // Automation curves.
    // A deck that isn't playing shows as fully off in every lane.
    let silent = DeckFx { gain: 0.0, eq: [0.0; 3], ..DeckFx::FLAT };
    let point = |ms: f64| -> (DeckFx, DeckFx) {
        if ms < 0.0 {
            (DeckFx::FLAT, silent)
        } else if ms >= timing.duration_ms {
            (silent, DeckFx::FLAT)
        } else {
            let p = t.at((ms / timing.duration_ms) as f32);
            (p.out, p.inc)
        }
    };
    let curve = |name: &str, f: Field, value: &dyn Fn(&DeckFx) -> f32| -> Line<'static> {
        let label_style = if selected(f) { bold(accent()) } else { subtle() };
        let mut spans = vec![Span::styled(format!("{name:<4}"), label_style)];
        for c in 0..width.saturating_sub(2) {
            let (a, b) = point(col_time(c + 2));
            let (va, vb) = (value(&a).clamp(0.0, 1.0), value(&b).clamp(0.0, 1.0));
            spans.push(two_levels(va, vb));
        }
        Line::from(spans)
    };
    lines.push(Line::raw(""));
    lines.push(curve(" VOL", Field::Volume, &|d| d.gain));
    lines.push(curve(" LOW", Field::Eq, &|d| d.eq[0]));
    lines.push(curve(" FX", Field::Effect, &|d| d.filter.abs().max(d.echo).max(1.0 - d.speed)));
    lines.push(Line::from(vec![
        Span::styled("     ", dim()),
        Span::styled("▀ A (outgoing)", Style::new().fg(GREEN)),
        Span::styled("   ▄ B (incoming)", Style::new().fg(PURPLE)),
        Span::styled("   █ both", Style::new().fg(TEXT)),
    ]));
    lines.push(Line::raw(""));

    // Fields.
    let field = |f: Field, name: &str, value: String, note: String| -> Line<'static> {
        let on = selected(f);
        let marker = if on { "▸ " } else { "  " };
        let value_style = if on { bold(accent()) } else { text() };
        Line::from(vec![
            Span::styled(format!(" {marker}{name:<8}"), if on { bold(text()) } else { subtle() }),
            Span::styled(if on { format!("‹ {value} ›") } else { format!("  {value}  ") }, value_style),
            Span::styled(format!("  {note}"), dim()),
        ])
    };
    let presets: String = Preset::ALL.iter().enumerate().map(|(i, p)| format!("{}{} ", i + 1, p.label())).collect();
    lines.push(field(Field::Preset, "Preset", t.preset.label().into(), if t.preset == Preset::Custom { "your own curves".into() } else { t.preset.description().into() }));
    lines.push(field(Field::Length, "Length", bars(t.bars), format!("{:.1} s", timing.duration_ms / 1000.0)));
    let sync_note = match (t.sync, ga.zip(gb)) {
        (true, Some(_)) if (timing.rate - 1.0).abs() > 1e-6 => format!("B plays at {:.1}% to lock the beats, then eases back", timing.rate * 100.0),
        (true, Some(_)) => "same tempo, beats locked".into(),
        (true, None) => "needs both tempos".into(),
        (false, _) => "tempos left alone".into(),
    };
    lines.push(field(Field::Sync, "Sync", if t.sync { "on".into() } else { "off".into() }, sync_note));
    lines.push(field(Field::Out, "A out", clock_tenths(t.out_ms), "h/l beat · H/L bar · w/b phrase · ,/. 10 ms".into()));
    lines.push(field(Field::In, "B in", clock_tenths(t.in_ms), "h/l beat · H/L bar · w/b phrase · ,/. 10 ms".into()));
    lines.push(field(Field::Volume, "Volume", t.volume.label().into(), String::new()));
    lines.push(field(Field::Eq, "EQ", t.eq.label().into(), String::new()));
    lines.push(field(Field::Effect, "Effect", t.effect.label().into(), String::new()));
    lines.push(Line::raw(""));
    lines.push(Line::styled(format!(" {presets}· 0 automatic"), dim()));
    lines.push(Line::styled(" j/k field · h/l change · space preview loop · [ ] other transitions · s sync · +/- length · esc close", dim()));

    frame.render_widget(Paragraph::new(lines).style(Style::new().bg(PANEL)), area);
}

/// One cell showing two levels: the upper half for A, the lower for B.
fn two_levels(a: f32, b: f32) -> Span<'static> {
    let (a_on, b_on) = (a >= 0.5, b >= 0.5);
    let shade = |v: f32, color: Color| {
        let level = 0.25 + 0.75 * v;
        match color {
            Color::Rgb(r, g, bl) => Color::Rgb((f32::from(r) * level) as u8, (f32::from(g) * level) as u8, (f32::from(bl) * level) as u8),
            c => c,
        }
    };
    match (a_on, b_on) {
        (true, true) => Span::styled("█", Style::new().fg(TEXT)),
        (true, false) => Span::styled("▀", Style::new().fg(shade(a, GREEN)).bg(if b > 0.05 { shade(b, PURPLE) } else { PANEL })),
        (false, true) => Span::styled("▄", Style::new().fg(shade(b, PURPLE)).bg(if a > 0.05 { shade(a, GREEN) } else { PANEL })),
        (false, false) => {
            let glyph = if a > 0.05 || b > 0.05 { "·" } else { " " };
            Span::styled(glyph, Style::new().fg(DIM))
        }
    }
}

/// `33.1` for the first beat of bar 33.
fn beat_label(grid: Option<Grid>, ms: f64) -> String {
    match grid {
        Some(g) => {
            let beat = g.beat_at(ms);
            let whole = beat.round();
            let bar = (whole / 4.0).floor() as i64 + 1;
            let in_bar = whole.rem_euclid(4.0) as i64 + 1;
            let off = (beat - whole) * g.beat_ms();
            if off.abs() >= 1.0 { format!("{bar}.{in_bar} {off:+.0} ms") } else { format!("{bar}.{in_bar}") }
        }
        None => "no grid".into(),
    }
}

/// Where a looping preview is, in ms from the start of the window.
fn preview_time(p: &PlayerState, t: &Transition, timing: &vimj_core::transition::Timing) -> Option<f64> {
    match p.index {
        Some(0) => Some(p.position_ms - t.out_ms),
        Some(1) => Some(timing.duration_ms + (p.position_ms - timing.in_end_ms)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_beats_in_bars() {
        let g = Some(Grid { bpm: 120.0, downbeat_ms: 0.0 });
        assert_eq!(beat_label(g, 0.0), "1.1");
        assert_eq!(beat_label(g, 2_500.0), "2.2");
        assert_eq!(beat_label(g, 2_520.0), "2.2 +20 ms");
        assert_eq!(beat_label(None, 0.0), "no grid");
    }

    #[test]
    fn cycles_wrap_both_ways() {
        assert_eq!(cycle(&Volume::ALL, Volume::Smooth, -1), Volume::CenterCut);
        assert_eq!(cycle(&Volume::ALL, Volume::CenterCut, 1), Volume::Smooth);
    }
}
