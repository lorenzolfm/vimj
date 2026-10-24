//! The transition editor: both songs' waveforms lined up on one timeline,
//! each with its volume, EQ, filter and effect curves drawn over it in one
//! colour per kind, as Spotify's editor has them, and a field list
//! edited the vim way: `j`/`k` pick a field, `h`/`l` change it. `c` draws a
//! field's curves by hand, point by point, as Spotify's Customise does.
//! Edits stay in a draft until `:w` stores them, as in a vim buffer, and `u`
//! and `C-r` step back and forth through them.

use crate::mouse::{self, Hits, Target};
use crate::theme::{self, *};
use crate::wave;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use vimj_core::curve::{self, Curve, Point};
use vimj_core::model::{BEATS_PER_BAR, Grid};
use vimj_core::protocol::{PlayerState, SongInfo, TransitionDetail};
use vimj_core::transition::{BAR_CHOICES, Effect, Eq, Filter, FilterKind, Lane, Looping, Preset, Side, Track, Transition, Volume};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

/// A running preview picks up edits this long after the last one.
const PREVIEW_AFTER: Duration = Duration::from_millis(300);
/// Fine nudge for lining beats up by ear.
const NUDGE_MS: f64 = 10.0;
/// The outgoing song plays this long before the window when previewing.
pub const LEAD_IN_MS: f64 = 4_000.0;
/// Edits `u` can take back.
const HISTORY: usize = 100;
/// `j`/`k` change a curve point's value by this much, `J`/`K` by the jump.
const VALUE_STEP: f32 = 0.05;
const VALUE_JUMP: f32 = 0.25;
/// `,`/`.` move a curve point by this much of the window.
const POINT_NUDGE: f32 = 0.01;
/// `<`/`>` bend a segment by this much.
const BEND_STEP: f32 = 0.1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Preset,
    Length,
    Sync,
    Out,
    In,
    Volume,
    Eq,
    Filter,
    Effect,
    Looping,
}

const FIELDS: [Field; 10] =
    [Field::Preset, Field::Length, Field::Sync, Field::Out, Field::In, Field::Volume, Field::Eq, Field::Filter, Field::Effect, Field::Looping];

impl Field {
    /// The curves a field picks the style of.
    pub fn lane(self) -> Option<Lane> {
        match self {
            Field::Volume => Some(Lane::Volume),
            Field::Eq => Some(Lane::Eq),
            Field::Filter => Some(Lane::Filter),
            Field::Effect => Some(Lane::Effect),
            _ => None,
        }
    }

    fn of(lane: Lane) -> Field {
        match lane {
            Lane::Volume => Field::Volume,
            Lane::Eq => Field::Eq,
            Lane::Filter => Field::Filter,
            Lane::Effect => Field::Effect,
        }
    }
}

/// A curve being drawn by hand: one lane of one song, and the point picked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Drawing {
    pub lane: Lane,
    pub track: Track,
    pub point: usize,
}

/// What the keys work on: a field, or a curve being drawn by hand, whose
/// lane's field is then the one picked.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Fields(Field),
    Drawing(Drawing),
}

pub struct Editor {
    /// The pair is `songs[index]` → `songs[index + 1]`.
    pub index: usize,
    pub count: usize,
    from: String,
    to: String,
    /// The pair as stored.
    pub detail: Option<TransitionDetail>,
    /// The transition being edited, which `:w` stores.
    pub draft: Option<Transition>,
    mode: Mode,
    /// Drafts before each edit, and those `u` stepped back over.
    undo: Vec<Transition>,
    redo: Vec<Transition>,
    /// The draft as a mouse drag of a curve point found it, until the drag
    /// changes it: that is then the drag's one undo step.
    drag_from: Option<Transition>,
    /// When the draft last changed, until a running preview hears of it.
    changed_at: Option<Instant>,
    /// The last edit moved the window, so a running preview should start over.
    restart_preview: bool,
    pub previewing: bool,
}

pub enum Action {
    None,
    Close,
    Command,
    TogglePreview,
    /// Move to the pair this many places away.
    Pair(isize),
    Error(String),
}

impl Editor {
    pub fn new(index: usize, count: usize, from: String, to: String) -> Editor {
        Editor {
            index,
            count,
            from,
            to,
            detail: None,
            draft: None,
            mode: Mode::Fields(Field::Preset),
            undo: Vec::new(),
            redo: Vec::new(),
            drag_from: None,
            changed_at: None,
            restart_preview: false,
            previewing: false,
        }
    }

    /// The field picked; while drawing, the one whose curve it is.
    pub fn field(&self) -> Field {
        match self.mode {
            Mode::Fields(f) => f,
            Mode::Drawing(d) => Field::of(d.lane),
        }
    }

    /// Picks a field. A curve being drawn keeps its own.
    pub fn pick_field(&mut self, field: Field) {
        if let Mode::Fields(f) = &mut self.mode {
            *f = field;
        }
    }

    /// The curve being drawn by hand, if one is.
    pub fn drawing(&self) -> Option<Drawing> {
        match self.mode {
            Mode::Drawing(d) => Some(d),
            Mode::Fields(_) => None,
        }
    }

    pub fn pair(&self) -> (String, String) {
        (self.from.clone(), self.to.clone())
    }

    pub fn is_pair(&self, detail: &TransitionDetail) -> bool {
        detail.from.song.id == self.from && detail.to.song.id == self.to
    }

    pub fn loaded(&mut self, detail: TransitionDetail) {
        if !self.is_pair(&detail) {
            return;
        }
        if self.draft.is_none() {
            self.draft = Some(detail.transition.clone());
        }
        self.detail = Some(detail);
    }

    /// `sent` was stored as `detail`. The draft takes on what the daemon
    /// made of it, unless it has been edited again since.
    pub fn saved(&mut self, detail: TransitionDetail, sent: &Transition) {
        if !self.is_pair(&detail) {
            return;
        }
        if self.draft.as_ref() == Some(sent) {
            self.draft = Some(detail.transition.clone());
        }
        self.detail = Some(detail);
    }

    /// The draft differs from what is stored.
    pub fn modified(&self) -> bool {
        matches!((&self.detail, &self.draft), (Some(d), Some(t)) if d.transition != *t)
    }

    /// Throws the edits away.
    pub fn revert(&mut self) {
        if let Some(d) = &self.detail {
            self.replace(d.transition.clone());
        }
    }

    /// Goes back to the transition vimj would pick by itself.
    pub fn reset(&mut self) {
        if let Some(d) = &self.detail {
            self.replace(d.automatic.clone());
        }
    }

    fn replace(&mut self, t: Transition) {
        if self.draft.as_ref() == Some(&t) {
            return;
        }
        if let Some(before) = self.draft.replace(t) {
            self.remember(before);
        }
        self.changed(true);
    }

    fn remember(&mut self, before: Transition) {
        self.undo.push(before);
        if self.undo.len() > HISTORY {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn changed(&mut self, moves_window: bool) {
        self.changed_at = Some(Instant::now());
        self.restart_preview |= moves_window;
        self.fit_drawing();
    }

    /// Takes back the last edit.
    pub fn undo(&mut self) {
        if let Some(before) = self.undo.pop() {
            self.redo.extend(self.draft.replace(before));
            self.changed(true);
        }
    }

    /// Makes an edit `u` took back again.
    pub fn redo(&mut self) {
        if let Some(after) = self.redo.pop() {
            self.undo.extend(self.draft.replace(after));
            self.changed(true);
        }
    }

    /// Starts previewing; returns what to play.
    pub fn start_preview(&mut self) -> Option<Transition> {
        let t = self.draft.clone()?;
        self.previewing = true;
        self.changed_at = None;
        self.restart_preview = false;
        Some(t)
    }

    /// The draft for a running preview to play, once edits have settled,
    /// and whether the preview should start over.
    pub fn preview_due(&mut self) -> Option<(Transition, bool)> {
        if !self.previewing || self.changed_at?.elapsed() < PREVIEW_AFTER {
            return None;
        }
        self.changed_at = None;
        Some((self.draft.clone()?, std::mem::take(&mut self.restart_preview)))
    }

    fn grids(&self) -> (Option<Grid>, Option<Grid>) {
        let grid = |s: &SongInfo| s.song.analysis.as_ref().and_then(|a| a.grid);
        self.detail.as_ref().map_or((None, None), |d| (grid(&d.from), grid(&d.to)))
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char(':') => return Action::Command,
            KeyCode::Char(' ') => return Action::TogglePreview,
            KeyCode::Char('u') => self.undo(),
            KeyCode::Char('r') if ctrl => self.redo(),
            _ if self.drawing().is_some() => self.drawing_key(key),
            _ => return self.field_key(key),
        }
        Action::None
    }

    fn field_key(&mut self, key: KeyEvent) -> Action {
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => return Action::Close,
            KeyCode::Char('[') => return Action::Pair(-1),
            KeyCode::Char(']') => return Action::Pair(1),
            KeyCode::Char('c') => return self.customize(),
            KeyCode::Char('R') | KeyCode::Char('0') => self.reset(),
            KeyCode::Char('j') | KeyCode::Down | KeyCode::Tab => self.step_field(1),
            KeyCode::Char('k') | KeyCode::Up | KeyCode::BackTab => self.step_field(-1),
            KeyCode::Char('g') => self.pick_field(FIELDS[0]),
            KeyCode::Char('G') => self.pick_field(FIELDS[FIELDS.len() - 1]),
            KeyCode::Char(c @ '1'..='9') => self.preset(Preset::ALL[c as usize - '1' as usize]),
            KeyCode::Char('s') => self.edit(true, |t| t.sync = !t.sync),
            KeyCode::Char('+') | KeyCode::Char('=') => self.change(Field::Length, 1, false),
            KeyCode::Char('-') => self.change(Field::Length, -1, false),
            KeyCode::Char('h') | KeyCode::Left => self.change(self.field(), -1, shift),
            KeyCode::Char('l') | KeyCode::Right => self.change(self.field(), 1, shift),
            KeyCode::Char('H') => self.change(self.field(), -1, true),
            KeyCode::Char('L') => self.change(self.field(), 1, true),
            KeyCode::Char('b') => self.change(self.field(), -(PHRASE_STEPS as i32), true),
            KeyCode::Char('w') => self.change(self.field(), PHRASE_STEPS as i32, true),
            KeyCode::Char(',') => self.nudge(-NUDGE_MS),
            KeyCode::Char('.') => self.nudge(NUDGE_MS),
            _ => {}
        }
        Action::None
    }

    fn drawing_key(&mut self, key: KeyEvent) {
        let beats = BEATS_PER_BAR as f32;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('c') | KeyCode::Enter => self.mode = Mode::Fields(self.field()),
            KeyCode::Tab | KeyCode::BackTab => self.switch_track(),
            KeyCode::Char('w') | KeyCode::Char('e') => self.step_point(1),
            KeyCode::Char('b') => self.step_point(-1),
            KeyCode::Char('0') | KeyCode::Char('^') | KeyCode::Home => self.pick_point(0),
            KeyCode::Char('$') | KeyCode::End => self.pick_point(usize::MAX),
            KeyCode::Char('h') | KeyCode::Left => self.move_by_beats(-1.0),
            KeyCode::Char('l') | KeyCode::Right => self.move_by_beats(1.0),
            KeyCode::Char('H') => self.move_by_beats(-beats),
            KeyCode::Char('L') => self.move_by_beats(beats),
            KeyCode::Char(',') => self.move_point(-POINT_NUDGE, 0.0),
            KeyCode::Char('.') => self.move_point(POINT_NUDGE, 0.0),
            KeyCode::Char('k') | KeyCode::Up => self.raise(VALUE_STEP),
            KeyCode::Char('j') | KeyCode::Down => self.raise(-VALUE_STEP),
            KeyCode::Char('K') => self.raise(VALUE_JUMP),
            KeyCode::Char('J') => self.raise(-VALUE_JUMP),
            KeyCode::Char('o') | KeyCode::Char('a') => self.add_point(),
            KeyCode::Char('x') | KeyCode::Char('d') | KeyCode::Delete | KeyCode::Backspace => self.delete_point(),
            KeyCode::Char('s') => self.edit_curve(true, |c, i| c.toggle_linear(*i)),
            KeyCode::Char('<') => self.edit_curve(true, |c, i| c.bend(*i, -BEND_STEP)),
            KeyCode::Char('>') => self.edit_curve(true, |c, i| c.bend(*i, BEND_STEP)),
            KeyCode::Char('R') => self.reset_curve(),
            _ => {}
        }
    }

    fn step_field(&mut self, by: isize) {
        let at = FIELDS.iter().position(|f| *f == self.field()).unwrap_or(0) as isize;
        self.pick_field(FIELDS[(at + by).rem_euclid(FIELDS.len() as isize) as usize]);
    }

    fn edit(&mut self, moves_window: bool, f: impl FnOnce(&mut Transition)) {
        self.edit_with(moves_window, true, f);
    }

    /// Applies an edit to the draft; `record` gives it its own undo step.
    fn edit_with(&mut self, moves_window: bool, record: bool, f: impl FnOnce(&mut Transition)) {
        let (Some(t), Some(d)) = (&self.draft, &self.detail) else { return };
        let mut after = t.clone();
        f(&mut after);
        after.auto = false;
        // Kept within both songs, as the daemon keeps what it stores.
        after.clamp(side(&d.from), side(&d.to));
        if (Transition { auto: t.auto, ..after.clone() }) == *t {
            return;
        }
        let before = self.draft.replace(after).expect("checked above");
        if record {
            self.remember(before);
        }
        self.changed(moves_window);
    }

    pub fn preset(&mut self, preset: Preset) {
        self.edit(true, |t| t.set_preset(preset));
    }

    /// `coarse` moves cue points by a bar instead of a beat.
    pub fn change(&mut self, field: Field, by: i32, coarse: bool) {
        let (a, b) = self.grids();
        let step = |g: Option<Grid>| beat_ms(g) * if coarse { f64::from(BEATS_PER_BAR) } else { 1.0 } * f64::from(by);
        match field {
            Field::Preset => self.edit(true, |t| {
                let at = t.preset().and_then(|p| Preset::ALL.iter().position(|q| *q == p)).map_or(0, |i| i as i32 + by);
                t.set_preset(Preset::ALL[at.rem_euclid(Preset::ALL.len() as i32) as usize]);
            }),
            Field::Length => self.edit(true, |t| {
                let at = BAR_CHOICES.iter().position(|b| *b >= t.bars).unwrap_or(BAR_CHOICES.len() - 1) as i32;
                t.bars = BAR_CHOICES[(at + by).clamp(0, BAR_CHOICES.len() as i32 - 1) as usize];
            }),
            Field::Sync => self.edit(true, |t| t.sync = !t.sync),
            Field::Out => self.edit(true, |t| t.out_ms = (t.out_ms + step(a)).max(0.0)),
            Field::In => self.edit(true, |t| t.in_ms = (t.in_ms + step(b)).max(0.0)),
            Field::Volume => self.edit(false, |t| t.set_volume(cycle(&Volume::ALL, t.volume, by))),
            Field::Eq => self.edit(false, |t| t.set_eq(cycle(&Eq::ALL, t.eq, by))),
            Field::Filter => self.edit(false, |t| t.set_filter(cycle(&Filter::ALL, t.filter, by))),
            Field::Effect => self.edit(false, |t| t.set_effect(cycle(&Effect::ALL, t.effect, by))),
            Field::Looping => self.edit(false, |t| t.looping = cycle(&Looping::ALL, t.looping, by)),
        }
    }

    fn nudge(&mut self, ms: f64) {
        match self.field() {
            Field::Out => self.edit(true, |t| t.out_ms = (t.out_ms + ms).max(0.0)),
            _ => self.edit(true, |t| t.in_ms = (t.in_ms + ms).max(0.0)),
        }
    }

    /// Where A leaves or B enters.
    pub fn cue(&self, field: Field) -> Option<f64> {
        let t = self.draft.as_ref()?;
        match field {
            Field::Out => Some(t.out_ms),
            Field::In => Some(t.in_ms),
            _ => None,
        }
    }

    /// Moves a cue point that was at `from_ms` by about `by_ms`, in whole
    /// beats so the beats stay lined up.
    pub fn drag(&mut self, field: Field, from_ms: f64, by_ms: f64) {
        let (a, b) = self.grids();
        let beat = beat_ms(if field == Field::Out { a } else { b });
        let to = (from_ms + (by_ms / beat).round() * beat).max(0.0);
        if self.cue(field).is_none_or(|now| now == to) {
            return;
        }
        match field {
            Field::Out => self.edit(true, |t| t.out_ms = to),
            _ => self.edit(true, |t| t.in_ms = to),
        }
    }

    // Drawing curves.

    /// Starts drawing the chosen field's curves by hand.
    pub fn customize(&mut self) -> Action {
        match self.field().lane() {
            Some(lane) => self.draw_lane(lane),
            None => Action::Error("Volume, EQ, Filter and Effects have curves to draw; pick one of them".into()),
        }
    }

    pub fn draw_lane(&mut self, lane: Lane) -> Action {
        let Some(t) = &self.draft else { return Action::None };
        let Some(track) = [Track::Out, Track::In].into_iter().find(|&k| t.curve(lane, k).is_some()) else {
            return Action::Error(format!("{} is None, so there is no curve to draw: pick a style first", lane.label()));
        };
        self.mode = Mode::Drawing(Drawing { lane, track, point: 0 });
        Action::None
    }

    /// The curve being drawn.
    pub fn points(&self) -> Option<&[Point]> {
        let d = self.drawing()?;
        self.draft.as_ref()?.curve(d.lane, d.track)
    }

    /// Keeps the drawing on a curve that exists, after the draft changed under it.
    fn fit_drawing(&mut self) {
        let (Mode::Drawing(mut d), Some(t)) = (self.mode, &self.draft) else { return };
        if t.curve(d.lane, d.track).is_none() {
            d.track = if d.track == Track::Out { Track::In } else { Track::Out };
        }
        self.mode = match t.curve(d.lane, d.track) {
            Some(points) => Mode::Drawing(Drawing { point: d.point.min(points.len() - 1), ..d }),
            None => Mode::Fields(Field::of(d.lane)),
        };
    }

    fn switch_track(&mut self) {
        let (Some(d), Some(t)) = (self.drawing(), &self.draft) else { return };
        let other = if d.track == Track::Out { Track::In } else { Track::Out };
        if t.curve(d.lane, other).is_some() {
            self.mode = Mode::Drawing(Drawing { track: other, ..d });
            self.fit_drawing();
        }
    }

    fn step_point(&mut self, by: isize) {
        let Some(d) = self.drawing() else { return };
        self.pick_point(d.point.saturating_add_signed(by));
    }

    fn pick_point(&mut self, i: usize) {
        let Some(len) = self.points().map(<[Point]>::len) else { return };
        if let Mode::Drawing(d) = &mut self.mode {
            d.point = i.min(len - 1);
        }
    }

    /// Redraws the curve being drawn; `record` gives the edit its own undo step.
    fn edit_curve(&mut self, record: bool, f: impl FnOnce(&mut Curve, &mut usize)) {
        let Some(Drawing { lane, track, mut point }) = self.drawing() else { return };
        self.edit_with(false, record, |t| {
            t.redraw(lane, track, |c| {
                f(c, &mut point);
                point = point.min(c.len() - 1);
            });
        });
        if let Mode::Drawing(d) = &mut self.mode {
            d.point = point;
        }
    }

    fn window_beats(&self) -> f32 {
        self.draft.as_ref().map_or(16.0, |t| (t.bars * BEATS_PER_BAR) as f32)
    }

    /// Moves the point by whole beats, landing on the beat.
    fn move_by_beats(&mut self, beats: f32) {
        let per = self.window_beats();
        self.edit_curve(true, |c, i| {
            let p = c[*i];
            c.move_point(*i, ((p.x * per).round() + beats) / per, p.y);
        });
    }

    fn move_point(&mut self, dx: f32, dy: f32) {
        self.edit_curve(true, |c, i| {
            let p = c[*i];
            c.move_point(*i, p.x + dx, p.y + dy);
        });
    }

    /// Moves the point `by` up the lane as it is drawn, which lowers the
    /// value of a low-pass, as that closes downwards.
    fn raise(&mut self, by: f32) {
        let (Some(d), Some(t)) = (self.drawing(), &self.draft) else { return };
        let dy = by * Scale::of(t, d.lane, d.track).up();
        self.move_point(0.0, dy);
    }

    /// Adds a point halfway to the next one, or to the previous from the last.
    fn add_point(&mut self) {
        self.edit_curve(true, |c, i| {
            let (a, b) = if *i + 1 < c.len() { (*i, *i + 1) } else { (*i - 1, *i) };
            if let Some(at) = c.insert((c[a].x + c[b].x) / 2.0) {
                *i = at;
            }
        });
    }

    fn delete_point(&mut self) {
        self.edit_curve(true, |c, i| {
            c.remove(*i);
        });
    }

    /// Goes back to the style's curve for this song.
    fn reset_curve(&mut self) {
        let Some(d) = self.drawing() else { return };
        self.edit(false, |t| t.reset_curve(d.lane, d.track));
    }

    /// A click on a song's curve at `x` through the window: picks the
    /// nearest point within `reach` of it, or on a double click adds one.
    /// Says whether a point is picked to drag.
    pub fn click_curve(&mut self, track: Track, x: f32, reach: f32, double: bool) -> bool {
        let Some(d) = self.drawing() else { return false };
        if d.track != track {
            if self.draft.as_ref().and_then(|t| t.curve(d.lane, track)).is_none() {
                return false;
            }
            self.mode = Mode::Drawing(Drawing { track, point: 0, ..d });
        }
        if double {
            self.edit_curve(true, |c, i| {
                if let Some(at) = c.insert(x) {
                    *i = at;
                }
            });
        } else {
            let Some(points) = self.points() else { return false };
            let nearest = (0..points.len()).min_by(|&a, &b| (points[a].x - x).abs().total_cmp(&(points[b].x - x).abs()));
            let Some(i) = nearest.filter(|&i| (points[i].x - x).abs() <= reach) else { return false };
            self.pick_point(i);
        }
        self.drag_from = self.draft.clone();
        true
    }

    /// Drags the picked point to (`x`, `y`), onto a beat or the middle when
    /// within `snap` of one. A whole drag is one undo step.
    pub fn drag_point(&mut self, x: f32, y: f32, snap: f32) {
        let per = self.window_beats();
        let on_beat = (x * per).round() / per;
        let x = [on_beat, 0.5].into_iter().find(|s| (s - x).abs() <= snap).unwrap_or(x);
        self.edit_curve(false, |c, i| c.move_point(*i, x, y));
        if let Some(from) = self.drag_from.take_if(|from| self.draft.as_ref() != Some(&*from)) {
            self.remember(from);
        }
    }
}

fn side(s: &SongInfo) -> Side<'_> {
    Side { duration_ms: s.song.duration_ms as f64, analysis: s.song.analysis.as_ref() }
}

/// Songs not analysed yet move by half a second, a beat at 120 BPM.
fn beat_ms(grid: Option<Grid>) -> f64 {
    grid.map_or(500.0, |g| g.beat_ms())
}

/// `w`/`b` step a phrase of eight bars.
const PHRASE_STEPS: u32 = 8;

fn cycle<T: Copy + PartialEq>(all: &[T], current: T, by: i32) -> T {
    let at = all.iter().position(|x| *x == current).unwrap_or(0) as i32;
    all[(at + by).rem_euclid(all.len() as i32) as usize]
}

/// What a lane's value means for one song, as in "80% volume".
fn measure(t: &Transition, lane: Lane, track: Track) -> &'static str {
    match lane {
        Lane::Volume => "volume",
        Lane::Eq => "bass",
        Lane::Filter => match t.filter.kind(track) {
            Some(FilterKind::LowPass) => "low-pass closed",
            Some(FilterKind::HighPass) => "high-pass closed",
            Some(FilterKind::Noise) | None => "noise",
        },
        Lane::Effect => match t.effect.kind() {
            Some(vimj_core::transition::EffectKind::Reverb) => "reverb send",
            Some(_) | None => "echo send",
        },
    }
}

fn track_name(track: Track) -> &'static str {
    match track {
        Track::Out => "A",
        Track::In => "B",
    }
}

/// Which songs have a lane drawn by hand: "A", "B", "A and B" or "".
fn custom_on(t: &Transition, lane: Lane) -> &'static str {
    match (t.is_custom(lane, Track::Out), t.is_custom(lane, Track::In)) {
        (true, true) => "A and B",
        (true, false) => "A",
        (false, true) => "B",
        (false, false) => "",
    }
}

/// Braille dots by column (left, right) and row from the top.
const DOTS: [[u8; 4]; 2] = [[0x01, 0x02, 0x04, 0x40], [0x08, 0x10, 0x20, 0x80]];

/// Rows of each song's waveform: at least, and at most.
const MIN_LANE_ROWS: usize = 4;
const MAX_LANE_ROWS: usize = 12;
/// Rows the editor takes besides the two waveforms.
const OTHER_ROWS: usize = 18;
/// The timeline shows this much of the window's length either side of it,
/// and at least `MIN_PAD_MS`.
const PAD: f64 = 0.25;
const MIN_PAD_MS: f64 = 2_000.0;
/// The fields, in two columns under the waveforms.
const COLUMNS: [[Field; 5]; 2] = [
    [Field::Preset, Field::Length, Field::Sync, Field::Out, Field::In],
    [Field::Volume, Field::Eq, Field::Filter, Field::Effect, Field::Looping],
];
/// Where the second column of fields starts.
const SECOND_COLUMN: usize = 30;

fn lane_color(lane: Lane) -> Color {
    match lane {
        Lane::Volume => VOLUME,
        Lane::Eq => EQ,
        Lane::Filter => FILTER,
        Lane::Effect => EFFECT,
    }
}

/// Where a lane's values sit up a song's waveform, as fractions of its
/// height: value 0 at `zero` and 1 at `one`. Volume and effects fill it
/// from the bottom. EQ rests in the middle and cuts down from there. The
/// filter is open in the middle, a high-pass closing upwards and a low-pass
/// downwards, as on a DJ mixer's one filter knob.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scale {
    zero: f32,
    one: f32,
}

impl Scale {
    pub fn of(t: &Transition, lane: Lane, track: Track) -> Scale {
        match lane {
            Lane::Volume | Lane::Effect => Scale { zero: 0.0, one: 1.0 },
            Lane::Eq => Scale { zero: 0.0, one: 0.5 },
            Lane::Filter if t.filter.kind(track) == Some(FilterKind::LowPass) => Scale { zero: 0.5, one: 0.0 },
            Lane::Filter => Scale { zero: 0.5, one: 1.0 },
        }
    }

    fn height(self, value: f32) -> f32 {
        self.zero + value.clamp(0.0, 1.0) * (self.one - self.zero)
    }

    /// The value at `height`; past either end, that end's.
    pub fn value(self, height: f32) -> f32 {
        ((height - self.zero) / (self.one - self.zero)).clamp(0.0, 1.0)
    }

    /// 1 where higher values sit higher up, -1 where they sit lower.
    fn up(self) -> f32 {
        (self.one - self.zero).signum()
    }
}

/// Where the timeline's columns fall.
struct Timeline {
    width: usize,
    /// Ms from the start of the window at the left edge of column 0.
    t0: f64,
    ms_per_col: f64,
    duration_ms: f64,
    head_col: Option<isize>,
}

impl Timeline {
    /// Ms from the start of the window at `c` columns in.
    fn time(&self, c: f64) -> f64 {
        self.t0 + c * self.ms_per_col
    }

    /// Progress through the window at a point across column `c`.
    fn x(&self, c: usize, across: f64) -> f64 {
        self.time(c as f64 + across) / self.duration_ms
    }

    fn column(&self, x: f32) -> isize {
        ((f64::from(x) * self.duration_ms - self.t0) / self.ms_per_col).floor() as isize
    }

    fn in_window(&self, c: usize) -> bool {
        (0.0..self.duration_ms).contains(&self.time(c as f64 + 0.5))
    }
}

/// A curve drawn over a song's waveform: its height through the window,
/// 0 at the bottom and 1 at the top.
struct Trace<'a> {
    color: Color,
    height: Box<dyn Fn(f32) -> f32 + 'a>,
}

/// A curve's point while drawing it, at `x` through the window.
struct Mark {
    x: f32,
    height: f32,
    glyph: &'static str,
    style: Style,
}

/// The curves over one song, topmost first: the field picked, then the
/// rest. While drawing, the curves not being drawn are dimmed.
fn traces(t: &Transition, track: Track, focus: Option<Lane>, drawing: bool) -> Vec<Trace<'_>> {
    let order = [Lane::Volume, Lane::Filter, Lane::Eq, Lane::Effect];
    let mut traces = Vec::new();
    for lane in focus.into_iter().chain(order.into_iter().filter(|&l| Some(l) != focus)) {
        let Some(points) = t.curve(lane, track) else { continue };
        let drawn = drawing && focus == Some(lane);
        // EQ left flat on a song says nothing, unless it's being drawn.
        if lane == Lane::Eq && !drawn && points.iter().all(|p| p.y >= 1.0) {
            continue;
        }
        let scale = Scale::of(t, lane, track);
        let color = if drawing && !drawn { mix(lane_color(lane), WINDOW, 0.6) } else { lane_color(lane) };
        traces.push(Trace { color, height: Box::new(move |x| scale.height(curve::value(points, x))) });
    }
    // The EQ's mid and high bands, fainter, under the rest.
    let scale = Scale::of(t, Lane::Eq, track);
    for band in 1..3 {
        let value = move |x: f32| {
            let m = t.at(x);
            if track == Track::Out { m.out.eq[band] } else { m.inc.eq[band] }
        };
        if (0..=32).all(|i| value(i as f32 / 32.0) >= 1.0) {
            continue;
        }
        let color = mix(EQ, WINDOW, if drawing { 0.75 } else { 0.55 });
        traces.push(Trace { color, height: Box::new(move |x| scale.height(value(x))) });
    }
    traces
}

/// A song's waveform, mirrored about its middle, with `traces` drawn over
/// it and `marks` over those. `position` is where in the song each ms of the
/// timeline falls, and `heard` whether it plays then.
fn song_rows(
    tl: &Timeline,
    wave: Option<&Arc<Vec<u8>>>,
    position: &dyn Fn(f64) -> f64,
    heard: &dyn Fn(f64) -> bool,
    traces: &[Trace],
    marks: &[Mark],
    rows: usize,
) -> Vec<Line<'static>> {
    let dots = rows * 4;
    // In eighths of a row from the bottom.
    let middle = rows * 4;
    let level = |h: f32| (h.clamp(0.0, 1.0) * (dots - 1) as f32).round() as usize;

    // Each trace's braille dots, by column and then row.
    let plots: Vec<Vec<Vec<u8>>> = traces
        .iter()
        .map(|trace| {
            let mut cells = vec![vec![0u8; rows]; tl.width];
            let mut prev: Option<usize> = None;
            for (c, column) in cells.iter_mut().enumerate() {
                for (half, bits) in DOTS.iter().enumerate() {
                    let x = tl.x(c, 0.25 + 0.5 * half as f64);
                    if !(0.0..=1.0).contains(&x) {
                        prev = None;
                        continue;
                    }
                    let now = level((trace.height)(x as f32));
                    // Join to the last dot so that steep parts stay unbroken.
                    for d in prev.map_or(now, |p| p.min(now))..=prev.map_or(now, |p| p.max(now)) {
                        let from_top = dots - 1 - d;
                        column[from_top / 4] |= bits[from_top % 4];
                    }
                    prev = Some(now);
                }
            }
            cells
        })
        .collect();
    let marks: Vec<(usize, usize, &Mark)> = marks
        .iter()
        .map(|m| (tl.column(m.x).clamp(0, tl.width.saturating_sub(1) as isize) as usize, (dots - 1 - level(m.height)) / 4, m))
        .collect();
    // Each column's background, waveform colour, and how far the waveform
    // reaches either side of the middle.
    let columns: Vec<(Color, Color, usize)> = (0..tl.width)
        .map(|c| {
            let start = tl.time(c as f64);
            let (from, to) = (position(start), position(tl.time(c as f64 + 1.0)));
            let column = wave.and_then(|w| wave::column(w, from.min(to), from.max(to)));
            let base = if tl.in_window(c) { WINDOW } else { PANEL };
            let color = column.map_or(FAINT, |col| wave::color(col, heard(start)));
            // Short of the top and bottom rows, where full and silent curves run.
            let reach = column.map_or(0, |col| ((col.level * (middle - 8) as f32).round() as usize).max(1));
            (base, color, reach)
        })
        .collect();

    (0..rows)
        .map(|r| {
            let mut spans = vec![Span::raw("  ")];
            for (c, &(base, color, reach)) in columns.iter().enumerate() {
                let bottom = (rows - 1 - r) * 8;
                // Eighths of the cell the waveform covers: from the cell's
                // bottom above the middle, from its top below.
                let (cover, above) = if bottom >= middle {
                    ((middle + reach).saturating_sub(bottom).min(8), true)
                } else {
                    ((bottom + 8).saturating_sub(middle.saturating_sub(reach).max(bottom)), false)
                };
                let under = if cover >= 4 { color } else { base };
                let mark = marks.iter().find(|m| m.0 == c && m.1 == r).map(|m| m.2);
                let trace = plots.iter().position(|p| p[c][r] != 0);
                let span = if tl.head_col == Some(c as isize) {
                    Span::styled("│", Style::new().fg(TEXT).bg(base))
                } else if let Some(m) = mark {
                    Span::styled(m.glyph, m.style.bg(under))
                } else if let Some(k) = trace {
                    let glyph = char::from_u32(0x2800 + u32::from(plots[k][c][r])).unwrap_or(' ');
                    Span::styled(glyph.to_string(), Style::new().fg(traces[k].color).bg(under))
                } else if cover == 0 {
                    Span::styled(" ", Style::new().bg(base))
                } else if above {
                    Span::styled(BLOCKS[cover].to_string(), Style::new().fg(color).bg(base))
                } else {
                    // The top `cover` eighths: the rest drawn in the background over the waveform's colour.
                    Span::styled(BLOCKS[8 - cover].to_string(), Style::new().fg(base).bg(color))
                };
                spans.push(span);
            }
            Line::from(spans)
        })
        .collect()
}

/// A song's beats along the timeline, bars taller; `↺` where a loop jumps
/// back. `down` points the beats down, for the ruler over a waveform.
fn ruler(
    tl: &Timeline,
    grid: Option<Grid>,
    position: &dyn Fn(f64) -> f64,
    heard: &dyn Fn(f64) -> bool,
    seam: &dyn Fn(f64, f64) -> bool,
    down: bool,
) -> Line<'static> {
    let mut spans = vec![Span::raw("  ")];
    for c in 0..tl.width {
        let (start, end) = (tl.time(c as f64), tl.time(c as f64 + 1.0));
        let (from, to) = (position(start), position(end));
        let beat = grid.filter(|_| to > 0.0).and_then(|g| {
            let first = g.beat_at(from).ceil();
            (g.beat_time(first) < to).then_some(first.rem_euclid(f64::from(BEATS_PER_BAR)) == 0.0)
        });
        let on = heard(start);
        let (glyph, color) = match beat {
            _ if seam(start, end) => ('↺', LOOPING),
            Some(true) => ('┃', if on { SUBTLE } else { DIM }),
            Some(false) => (if down { '╷' } else { '╵' }, if on { DIM } else { FAINT }),
            None => (' ', DIM),
        };
        spans.push(Span::styled(glyph.to_string(), Style::new().fg(color)));
    }
    Line::from(spans)
}

/// `left`, then `right` against the far edge if both fit in `width`.
fn spread(mut left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: usize) -> Line<'static> {
    let used = |spans: &[Span]| spans.iter().map(Span::width).sum::<usize>();
    let gap = width.saturating_sub(used(&left) + used(&right));
    if gap > 0 {
        left.push(Span::raw(" ".repeat(gap)));
        left.extend(right);
    }
    Line::from(left)
}

/// A song's name, where it leaves or enters, then its tempo and key on the right.
fn song_label(name: &str, s: &SongInfo, cue: String, picked: bool, width: usize) -> Line<'static> {
    let a = s.song.analysis.as_ref();
    let mut left = vec![Span::styled(format!(" {name}  "), bold(text())), Span::styled(s.song.title.clone(), bold(text()))];
    if !s.song.artist.is_empty() {
        left.push(Span::styled(format!(" — {}", s.song.artist), subtle()));
    }
    left.push(Span::styled(format!("   {cue}"), if picked { bold(accent()) } else { dim() }));
    let mut right = Vec::new();
    if let Some(bpm) = a.and_then(|a| a.bpm()) {
        right.push(Span::styled(format!("{bpm:.1} BPM"), subtle()));
    }
    if let Some(c) = theme::camelot(a) {
        right.push(Span::styled(format!("  {c}"), Style::new().fg(camelot_color(c))));
    }
    right.push(Span::raw(" "));
    spread(left, right, width)
}

/// Rows for each song's waveform out of `height`: even, so that it mirrors
/// about a line between two rows.
fn lane_rows(height: u16) -> usize {
    let rows = (usize::from(height).saturating_sub(OTHER_ROWS) / 2).clamp(MIN_LANE_ROWS, MAX_LANE_ROWS);
    rows - rows % 2
}

/// Draws the editor over `area`, noting what can be clicked in `hits`.
pub fn draw(frame: &mut Frame, area: Rect, editor: &Editor, waveforms: &HashMap<String, Arc<Vec<u8>>>, player: Option<&PlayerState>, hits: &mut Hits) {
    let (Some(detail), Some(t)) = (&editor.detail, &editor.draft) else {
        frame.render_widget(Paragraph::new(Line::styled("  Loading transition…", dim())), area);
        return;
    };
    let full = usize::from(area.width);
    let width = full.saturating_sub(4);
    let (ga, gb) = (detail.from.song.analysis.as_ref().and_then(|a| a.grid), detail.to.song.analysis.as_ref().and_then(|a| a.grid));
    let timing = t.timing(ga.as_ref(), gb.as_ref());
    let looping = t.loop_map(ga.as_ref());
    let rows = lane_rows(area.height);
    let drawing = editor.drawing();
    let focus = editor.field().lane();
    let selected = |f: Field| editor.field() == f;
    let mut lines: Vec<Line> = Vec::new();

    // Header: which transition, and how well the pair match.
    let header = vec![
        Span::styled(format!(" Transition {}/{}", editor.index + 1, editor.count.saturating_sub(1)), bold(accent())),
        Span::styled(if t.auto { "   automatic" } else { "   edited" }, dim()),
        Span::styled(if editor.modified() { "   [+] not saved, :w saves" } else { "" }, Style::new().fg(WARN)),
        Span::styled(if editor.previewing { "   ▶ previewing" } else { "" }, accent()),
    ];
    let mut matching = crate::ui::compat_spans(&detail.compat);
    matching.push(Span::raw(" "));
    lines.push(spread(header, matching, full));
    lines.push(Line::raw(""));

    // The timeline: the window, and some of each song either side.
    let pad = (timing.duration_ms * PAD).max(MIN_PAD_MS);
    let ms_per_col = (timing.duration_ms + 2.0 * pad) / width.max(1) as f64;
    let playhead = player.filter(|p| p.preview && editor.previewing).and_then(|p| preview_time(p, t, &timing));
    let head_col = playhead.map(|ms| ((ms + pad) / ms_per_col) as isize);
    let tl = Timeline { width, t0: -pad, ms_per_col, duration_ms: timing.duration_ms, head_col };

    // A plays through its loop, if it has one; the repeats show dimmer.
    let a_pos = |ms: f64| {
        let at = t.out_ms + ms;
        at + looping.as_ref().filter(|_| ms < timing.duration_ms).map_or(0.0, |m| m.offset(at))
    };
    let b_pos = |ms: f64| t.in_ms + ms * timing.rate;
    let a_heard = |ms: f64| ms < timing.duration_ms && !looping.as_ref().is_some_and(|m| m.repeating(t.out_ms + ms));
    let b_heard = |ms: f64| ms >= 0.0;
    let a_seam = |from: f64, to: f64| {
        to <= timing.duration_ms && looping.as_ref().is_some_and(|m| m.offset(t.out_ms + from) != m.offset(t.out_ms + to))
    };
    // The points of the curve being drawn, the one picked brightest.
    let marks = |track: Track| -> Vec<Mark> {
        let (Some(d), Some(points)) = (drawing, drawing.and_then(|d| t.curve(d.lane, track))) else { return Vec::new() };
        let scale = Scale::of(t, d.lane, track);
        let active = d.track == track;
        let mut marks: Vec<Mark> = points
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let (glyph, style) = match (active, i == d.point) {
                    (true, true) => ("◆", bold(accent())),
                    (true, false) => ("●", text()),
                    (false, _) => ("●", dim()),
                };
                Mark { x: p.x, height: scale.height(p.y), glyph, style }
            })
            .collect();
        // The picked point wins a cell it shares.
        marks.sort_by_key(|m| m.glyph != "◆");
        marks
    };
    // A song's waveform: its curve to pick points from while drawing,
    // otherwise its cue to drag.
    let lane_hits = |hits: &mut Hits, first: usize, track: Track, cue: Target| {
        let lane = mouse::rows(area, first, rows);
        match drawing.filter(|d| t.curve(d.lane, track).is_some()) {
            Some(d) => {
                let scale = Scale::of(t, d.lane, track);
                let target = Target::Curve { track, t0: tl.t0, ms_per_col: tl.ms_per_col, duration_ms: tl.duration_ms, scale };
                hits.add(mouse::columns(lane, 2, width), target);
            }
            None => hits.add(lane, cue),
        }
    };

    // A over B, as in Spotify, their curves drawn over them.
    let a_cue = Target::Lane(Field::Out, tl.ms_per_col);
    let first = lines.len();
    let cue = format!("leaves at {} · beat {}", clock_tenths(t.out_ms), beat_label(ga, t.out_ms));
    lines.push(song_label("A", &detail.from, cue, selected(Field::Out), full));
    lines.push(ruler(&tl, ga, &|ms| t.out_ms + ms, &a_heard, &a_seam, true));
    hits.add(mouse::rows(area, first, 2), a_cue);
    lane_hits(hits, lines.len(), Track::Out, a_cue);
    let a_traces = traces(t, Track::Out, focus, drawing.is_some());
    lines.extend(song_rows(&tl, waveforms.get(&detail.from.song.id), &a_pos, &a_heard, &a_traces, &marks(Track::Out), rows));

    // Between them, the window's length under its middle.
    let mut length = format!("{} · {:.1} s", bars(t.bars), timing.duration_ms / 1000.0);
    if t.sync && (timing.rate - 1.0).abs() > 1e-6 {
        length += &format!(" · B at {:.1}%", timing.rate * 100.0);
    }
    let middle = (timing.duration_ms / 2.0 - tl.t0) / tl.ms_per_col;
    let from = (middle - length.chars().count() as f64 / 2.0).max(0.0) as usize;
    let mut gap: Vec<Span> = vec![Span::raw(" "); width];
    for (cell, c) in gap.iter_mut().skip(from).zip(length.chars()) {
        *cell = Span::styled(c.to_string(), subtle());
    }
    if let Some(cell) = tl.head_col.and_then(|c| usize::try_from(c).ok()).and_then(|c| gap.get_mut(c)) {
        *cell = Span::styled("│", text());
    }
    gap.insert(0, Span::raw("  "));
    lines.push(Line::from(gap));

    let b_cue = Target::Lane(Field::In, tl.ms_per_col * timing.rate);
    lane_hits(hits, lines.len(), Track::In, b_cue);
    let b_traces = traces(t, Track::In, focus, drawing.is_some());
    lines.extend(song_rows(&tl, waveforms.get(&detail.to.song.id), &b_pos, &b_heard, &b_traces, &marks(Track::In), rows));
    let first = lines.len();
    lines.push(ruler(&tl, gb, &b_pos, &b_heard, &|_, _| false, false));
    let cue = format!("enters at {} · beat {}", clock_tenths(t.in_ms), beat_label(gb, t.in_ms));
    lines.push(song_label("B", &detail.to, cue, selected(Field::In), full));
    hits.add(mouse::rows(area, first, 2), b_cue);
    lines.push(Line::raw(""));

    // Presets, as buttons.
    let row = mouse::rows(area, lines.len(), 1);
    let current = t.preset();
    let mut presets = vec![Span::raw(" ")];
    let mut button = |presets: &mut Vec<Span<'static>>, label: &str, on: bool, target: Option<Target>| {
        let x: usize = presets.iter().map(Span::width).sum();
        let label = format!(" {label} ");
        if let Some(target) = target {
            hits.add(mouse::columns(row, x, label.width()), target);
        }
        let style = if on { bold(Style::new().fg(PANEL).bg(TEXT)) } else { Style::new().fg(SUBTLE).bg(HIGHLIGHT) };
        presets.push(Span::styled(label, style));
        presets.push(Span::raw(" "));
    };
    if current.is_none() {
        button(&mut presets, "Custom", true, None);
    }
    for (i, p) in Preset::ALL.iter().enumerate() {
        button(&mut presets, p.label(), current == Some(*p), Some(Target::Preset(i)));
    }
    let x: usize = presets.iter().map(Span::width).sum();
    hits.add(mouse::columns(row, x, " ✦ Auto ".width()), Target::Automatic);
    presets.push(Span::styled(" ✦ Auto ", if t.auto { bold(Style::new().fg(PANEL).bg(GREEN)) } else { Style::new().fg(SUBTLE).bg(HIGHLIGHT) }));
    lines.push(Line::from(presets));
    lines.push(Line::raw(""));

    // Fields, the curves' in their colours.
    let value = |f: Field| -> String {
        match f {
            Field::Preset => t.preset_label().into(),
            Field::Length => bars(t.bars),
            Field::Sync => if t.sync { "on" } else { "off" }.into(),
            Field::Out => clock_tenths(t.out_ms),
            Field::In => clock_tenths(t.in_ms),
            Field::Volume => t.volume.label().into(),
            Field::Eq => t.eq.label().into(),
            Field::Filter => t.filter.label().into(),
            Field::Effect => t.effect.label().into(),
            Field::Looping => t.looping.label().into(),
        }
    };
    let name = |f: Field| match f {
        Field::Preset => "Preset",
        Field::Length => "Length",
        Field::Sync => "Sync",
        Field::Out => "A out",
        Field::In => "B in",
        Field::Volume => "Volume",
        Field::Eq => "EQ",
        Field::Filter => "Filter",
        Field::Effect => "Effects",
        Field::Looping => "Looping",
    };
    for r in 0..COLUMNS[0].len() {
        let row = mouse::rows(area, lines.len(), 1);
        let mut spans: Vec<Span> = Vec::new();
        for (k, column) in COLUMNS.iter().enumerate() {
            let f = column[r];
            let x = if k == 0 { 1 } else { SECOND_COLUMN };
            let used: usize = spans.iter().map(Span::width).sum();
            spans.push(Span::raw(" ".repeat(x.saturating_sub(used).max(1))));
            let x: usize = spans.iter().map(Span::width).sum();
            let on = selected(f);
            let marker = match f.lane() {
                Some(lane) => Span::styled("▌ ", Style::new().fg(lane_color(lane))),
                None if f == Field::Looping => Span::styled("↺ ", Style::new().fg(LOOPING)),
                None => Span::raw("  "),
            };
            let label = Span::styled(format!("{:<9}", name(f)), if on { bold(text()) } else { subtle() });
            let shown = Span::styled(if on { format!("‹ {} ›", value(f)) } else { format!("  {}  ", value(f)) }, if on { bold(accent()) } else { text() });
            let lead = marker.width() + label.width();
            hits.add(mouse::columns(row, x, if k == 0 { SECOND_COLUMN - x } else { full.saturating_sub(x) }), Target::Field(f));
            if on {
                // `‹ value ›`: each arrow and the space beside it.
                hits.add(mouse::columns(row, x + lead, 2), Target::Step(f, -1));
                hits.add(mouse::columns(row, (x + lead + shown.width()).saturating_sub(2), 2), Target::Step(f, 1));
            }
            spans.extend([marker, label, shown]);
            if let Some(lane) = f.lane()
                && !custom_on(t, lane).is_empty()
            {
                spans.push(Span::styled(format!("✎ {}", custom_on(t, lane)), Style::new().fg(lane_color(lane))));
            }
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::raw(""));

    if let Some(d) = drawing {
        lines.push(point_line(t, d));
        lines.push(Line::styled(
            " w/b point · h/l beat · H/L bar · ,/. nudge · j/k value · o add · x delete · s eased/straight · </> bend · Tab A/B · R style's curve · u undo · esc done",
            dim(),
        ));
    } else {
        lines.push(Line::styled(format!(" {}", field_note(t, editor.field(), &timing, ga.zip(gb).is_some())), subtle()));
        lines.push(Line::styled(
            " j/k field · h/l change · 1-9 preset · 0 auto · c draw curves · space preview · u undo · [ ] other transitions · :w save · esc close",
            dim(),
        ));
    }

    frame.render_widget(Paragraph::new(lines).style(Style::new().bg(PANEL)), area);
}

/// What the picked field does, and how to change it.
fn field_note(t: &Transition, field: Field, timing: &vimj_core::transition::Timing, gridded: bool) -> String {
    let cue_keys = "h/l beat · H/L bar · w/b phrase · ,/. 10 ms · or drag the waveform";
    let curves = |lane: Lane, extra: &str| {
        let custom = custom_on(t, lane);
        let drawn = if custom.is_empty() { String::new() } else { format!("curve drawn by hand on {custom}") };
        let has_curve = [Track::Out, Track::In].iter().any(|&k| t.curve(lane, k).is_some());
        let hint = if has_curve { "c to draw your own" } else { "" };
        [extra, &drawn, hint].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ")
    };
    match field {
        Field::Preset => t.preset().map_or("Your own styles and curves", Preset::description).into(),
        Field::Length => format!("{:.1} s · +/- from any field", timing.duration_ms / 1000.0),
        Field::Sync => match (t.sync, gridded) {
            (true, true) if (timing.rate - 1.0).abs() > 1e-6 => {
                format!("B plays at {:.1}% to lock the beats, then eases back", timing.rate * 100.0)
            }
            (true, true) => "Same tempo, beats locked".into(),
            (true, false) => "Needs both tempos".into(),
            (false, _) => "Tempos left alone".into(),
        },
        Field::Out => format!("Where A starts handing over · {cue_keys}"),
        Field::In => format!("Where B comes in · {cue_keys}"),
        Field::Volume => curves(Lane::Volume, "Top is full volume"),
        Field::Eq => curves(Lane::Eq, "Middle is flat, down cuts the bass; fainter lines are the mids and highs"),
        Field::Filter => curves(Lane::Filter, "Middle is open, up closes a high-pass, down a low-pass"),
        Field::Effect => {
            let rings = match t.effect.kind() {
                Some(_) if t.effect.rings_out() => "On A, rising with the send, and rings on after the window",
                Some(_) => "On A, rising with the send, cut where the window ends",
                None => "",
            };
            curves(Lane::Effect, rings)
        }
        Field::Looping => match t.looping {
            Looping::None => "A plays straight through".into(),
            Looping::Halving => "A repeats from the window's start, half as long each time (↺ on its beats)".into(),
            _ => "A repeats from the window's start until B takes over (↺ on its beats)".into(),
        },
    }
}

/// The point picked on the curve being drawn.
fn point_line(t: &Transition, d: Drawing) -> Line<'static> {
    let head = Span::styled(format!(" Drawing {} on {}", d.lane.label(), track_name(d.track)), bold(Style::new().fg(lane_color(d.lane))));
    let Some(points) = t.curve(d.lane, d.track) else { return Line::from(head) };
    let i = d.point.min(points.len() - 1);
    let p = points[i];
    let ends = if i == 0 || i + 1 == points.len() { " · ends move up and down only" } else { "" };
    let segment = if i + 1 < points.len() {
        let bend = if p.bend == 0.0 { String::new() } else { format!(", bent {:+.0}%", p.bend * 100.0) };
        format!(" · {} to the next{bend}", if p.linear { "straight" } else { "eased" })
    } else {
        String::new()
    };
    let custom = if t.is_custom(d.lane, d.track) { "drawn by hand".to_owned() } else { format!("{}'s curve", style_name(t, d.lane)) };
    Line::from(vec![
        head,
        Span::styled(format!(" · {custom} · point {} of {}", i + 1, points.len()), bold(text())),
        Span::styled(format!(" · {:.0}% through · {:.0}% {}{segment}{ends}", p.x * 100.0, p.y * 100.0, measure(t, d.lane, d.track)), subtle()),
    ])
}

/// The style that drew a lane's curves.
fn style_name(t: &Transition, lane: Lane) -> &'static str {
    match lane {
        Lane::Volume => t.volume.label(),
        Lane::Eq => t.eq.label(),
        Lane::Filter => t.filter.label(),
        Lane::Effect => t.effect.label(),
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
        assert_eq!(cycle(&Volume::ALL, Volume::Smooth, -1), Volume::FadeInFastOut);
        assert_eq!(cycle(&Volume::ALL, Volume::FadeInFastOut, 1), Volume::Smooth);
    }

    /// Two four-minute songs at 120 BPM, so a beat is 500 ms, with the
    /// automatic transition between them.
    fn pair(from: &str, to: &str) -> TransitionDetail {
        let song = |id: &str| SongInfo {
            song: vimj_core::Song {
                id: id.into(),
                title: id.into(),
                artist: String::new(),
                duration_ms: 240_000,
                file: format!("{id}.opus"),
                source: None,
                added: 0,
                analysis: Some(vimj_core::Analysis {
                    version: 1,
                    grid: Some(Grid { bpm: 120.0, downbeat_ms: 0.0 }),
                    key: None,
                    energy: 0.5,
                    loudness: -9.0,
                    start_ms: 0.0,
                    end_ms: 240_000.0,
                }),
            },
            state: vimj_core::protocol::SongState::Ready,
        };
        let (from, to) = (song(from), song(to));
        let compat = vimj_core::compat::compat(from.song.analysis.as_ref(), to.song.analysis.as_ref());
        let automatic = vimj_core::transition::auto_transition(side(&from), side(&to), &compat);
        let grid = Some(Grid { bpm: 120.0, downbeat_ms: 0.0 });
        TransitionDetail { timing: automatic.timing(grid.as_ref(), grid.as_ref()), transition: automatic.clone(), saved: false, automatic, compat, from, to }
    }

    fn editor() -> Editor {
        let mut ed = Editor::new(0, 2, "a".into(), "b".into());
        ed.loaded(pair("a", "b"));
        ed
    }

    /// The daemon's answer to `:w`.
    fn stored(t: &Transition) -> TransitionDetail {
        TransitionDetail { transition: t.clone(), saved: true, ..pair("a", "b") }
    }

    fn press(ed: &mut Editor, keys: &str) {
        for c in keys.chars() {
            let modifiers = if c.is_ascii_uppercase() { KeyModifiers::SHIFT } else { KeyModifiers::NONE };
            ed.key(KeyEvent::new(KeyCode::Char(c), modifiers));
        }
    }

    #[test]
    fn edits_stay_unsaved_until_written() {
        let mut ed = editor();
        // A late answer about another pair changes nothing.
        ed.loaded(pair("b", "c"));
        assert_eq!(ed.detail.as_ref().map(|d| d.to.song.id.as_str()), Some("b"));
        assert!(!ed.modified());

        ed.change(Field::Volume, 1, false);
        assert!(ed.modified());
        let sent = ed.draft.clone().unwrap();
        ed.change(Field::Eq, 1, false);
        // Storing what was sent leaves the edit made since unsaved.
        ed.saved(stored(&sent), &sent);
        assert!(ed.modified());

        // The draft takes on whatever the daemon stored for it.
        let sent = ed.draft.clone().unwrap();
        let mut kept = sent.clone();
        kept.in_ms += 1.0;
        ed.saved(stored(&kept), &sent);
        assert_eq!(ed.draft.as_ref(), Some(&kept));
        assert!(!ed.modified());

        // Going back to automatic is an edit to write too; :e! undoes it.
        ed.reset();
        assert_eq!(ed.draft.as_ref(), Some(&pair("a", "b").automatic));
        assert!(ed.modified());
        ed.revert();
        assert_eq!(ed.draft.as_ref(), Some(&kept));
        assert!(!ed.modified());
    }

    #[test]
    fn undo_and_redo_step_through_the_edits() {
        let mut ed = editor();
        let start = ed.draft.clone().unwrap();
        ed.change(Field::Filter, 1, false);
        let filtered = ed.draft.clone().unwrap();
        ed.change(Field::Looping, 1, false);
        assert_eq!(ed.draft.as_ref().unwrap().looping, Looping::OneBeat);
        press(&mut ed, "u");
        assert_eq!(ed.draft.as_ref(), Some(&filtered));
        press(&mut ed, "u");
        assert_eq!(ed.draft.as_ref(), Some(&start));
        press(&mut ed, "u");
        assert_eq!(ed.draft.as_ref(), Some(&start), "nothing more to undo");
        ed.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert_eq!(ed.draft.as_ref(), Some(&filtered));
        // A new edit forgets what was undone.
        ed.change(Field::Effect, 1, false);
        ed.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert_eq!(ed.draft.as_ref().unwrap().looping, Looping::None);
    }

    #[test]
    fn edits_that_change_nothing_leave_it_automatic() {
        let mut ed = editor();
        // Moving a point that can't move on a curve nobody drew yet.
        ed.pick_field(Field::Volume);
        ed.customize();
        press(&mut ed, "h");
        assert!(!ed.modified() && ed.draft.as_ref().unwrap().auto);
    }

    #[test]
    fn drawing_a_curve_by_hand() {
        let mut ed = editor();
        // Blend: overlap, A's volume holds then fades from 85%.
        assert_eq!(ed.draft.as_ref().unwrap().preset(), Some(Preset::Blend));
        ed.pick_field(Field::Volume);
        assert!(matches!(ed.customize(), Action::None));
        assert_eq!(ed.drawing().map(|d| (d.lane, d.track, d.point)), Some((Lane::Volume, Track::Out, 0)));
        // To the middle point, down 25% twice, a bar earlier.
        press(&mut ed, "wJJH");
        let points = ed.points().unwrap().to_vec();
        assert_eq!(points.len(), 3);
        assert!((points[1].y - 0.5).abs() < 1e-6);
        // 0.85 of a 32-beat window, on the nearest beat (27), then 4 beats back.
        assert!((points[1].x - 23.0 / 32.0).abs() < 1e-6, "{}", points[1].x);
        let t = ed.draft.as_ref().unwrap();
        assert_eq!(t.preset(), None);
        assert!(t.is_custom(Lane::Volume, Track::Out) && !t.is_custom(Lane::Volume, Track::In));

        // Add one after it, make that segment straight, bend it, then take it out again.
        press(&mut ed, "os>");
        let added = ed.points().unwrap()[2];
        assert_eq!(ed.points().unwrap().len(), 4);
        assert!(added.linear && added.bend > 0.0);
        press(&mut ed, "x");
        assert_eq!(ed.points().unwrap().len(), 3);
        // The ends stay.
        press(&mut ed, "0x$x");
        assert_eq!(ed.points().unwrap().len(), 3);

        // B's curve is separate; R puts the style's back.
        ed.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(ed.drawing().unwrap().track, Track::In);
        ed.key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT));
        press(&mut ed, "R");
        assert!(ed.draft.as_ref().unwrap().curves.is_empty());

        // Esc goes back to the field drawn, though A's waveform was clicked
        // meanwhile, and the field keys work again.
        ed.pick_field(Field::Out);
        ed.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(ed.drawing().is_none());
        assert_eq!(ed.field(), Field::Volume);
        assert!(matches!(ed.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), Action::Close));
    }

    #[test]
    fn styles_without_curves_have_nothing_to_draw() {
        let mut ed = editor();
        ed.pick_field(Field::Effect);
        assert!(matches!(ed.customize(), Action::Error(_)), "Blend has no effect");
        ed.pick_field(Field::Sync);
        assert!(matches!(ed.customize(), Action::Error(_)));
        // A filter only on B starts on B.
        ed.pick_field(Field::Filter);
        ed.edit(false, |t| t.set_filter(Filter::LowPassIn));
        ed.customize();
        assert_eq!(ed.drawing().unwrap().track, Track::In);
        ed.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(ed.drawing().unwrap().track, Track::In, "A has no filter to switch to");
        // Undoing to a style with no curve stops the drawing.
        press(&mut ed, "u");
        assert!(ed.drawing().is_none());
    }

    #[test]
    fn mouse_picks_drags_and_adds_points() {
        let mut ed = editor();
        ed.pick_field(Field::Eq);
        ed.customize();
        // Blend's bass on A: full until 60%, gone at the end.
        assert!(ed.click_curve(Track::Out, 0.58, 0.03, false));
        assert_eq!(ed.drawing().unwrap().point, 1);
        ed.drag_point(0.3, 0.9, 0.0);
        ed.drag_point(0.49, 0.2, 0.02);
        let p = ed.points().unwrap()[1];
        assert!((p.x - 0.5).abs() < 1e-6 && (p.y - 0.2).abs() < 1e-6, "snapped to the middle: {p:?}");
        // One drag, one undo step.
        press(&mut ed, "u");
        assert!(ed.draft.as_ref().unwrap().curves.is_empty());
        assert!(!ed.click_curve(Track::Out, 0.3, 0.03, false), "nothing near");
        assert!(ed.click_curve(Track::Out, 0.3, 0.03, true));
        assert_eq!(ed.points().unwrap().len(), 4);
        assert_eq!(ed.drawing().unwrap().point, 1);
        // A drag that starts by changing nothing is still a step of its own.
        press(&mut ed, "0j");
        let lowered = ed.points().unwrap()[0].y;
        assert!(ed.click_curve(Track::Out, 0.0, 0.03, false));
        ed.drag_point(0.01, lowered, 0.0);
        ed.drag_point(0.01, 0.4, 0.0);
        press(&mut ed, "u");
        assert_eq!(ed.points().unwrap()[0].y, lowered, "back to before the drag, not before the j");
    }

    #[test]
    fn keys_move_a_point_up_and_down_the_lane() {
        let mut ed = editor();
        // Rise: a high-pass closing on A, which closes upwards, and a
        // low-pass opening on B, which closes downwards.
        ed.preset(Preset::Rise);
        ed.pick_field(Field::Filter);
        press(&mut ed, "c$k");
        assert!((ed.points().unwrap()[1].y - 0.85).abs() < 1e-6, "A's high-pass closes further");
        ed.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        press(&mut ed, "0k");
        assert!((ed.points().unwrap()[0].y - 0.75).abs() < 1e-6, "B's low-pass opens further");
        press(&mut ed, "J");
        assert!((ed.points().unwrap()[0].y - 1.0).abs() < 1e-6, "and closes going down");
    }

    #[test]
    fn draws_in_any_size_of_terminal() {
        let mut ed = editor();
        for drawing in [false, true] {
            if drawing {
                ed.pick_field(Field::Volume);
                ed.customize();
            }
            for (width, height) in [(0, 0), (10, 40), (29, 40), (100, 12), (160, 46)] {
                let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
                terminal.draw(|f| draw(f, f.area(), &ed, &HashMap::new(), None, &mut Hits::default())).unwrap();
            }
        }
    }

    #[test]
    fn dragging_moves_a_cue_by_whole_beats() {
        let mut ed = editor();
        let from = ed.cue(Field::In).unwrap();
        ed.drag(Field::In, from, 200.0);
        assert!(!ed.modified(), "less than half a beat moves nothing");
        ed.drag(Field::In, from, 740.0);
        assert_eq!(ed.cue(Field::In), Some(from + 500.0));

        // A nudge off the beat survives a drag.
        ed.pick_field(Field::In);
        ed.key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE));
        let nudged = from + 510.0;
        assert_eq!(ed.cue(Field::In), Some(nudged));
        ed.drag(Field::In, nudged, 1_100.0);
        assert_eq!(ed.cue(Field::In), Some(nudged + 1_000.0));
    }
}
