//! The mix engine: plays a set of songs on two decks and runs each
//! transition sample-accurately, beat-locked when the songs have grids.
//!
//! It does no IO. Decoded songs arrive through [`Command::Loaded`]; the
//! engine asks for the ones it needs through [`Engine::take_requests`].

use crate::deck::Deck;
use crate::dsp::{Smooth, soft_clip};
use crate::pcm::Pcm;
use vimj_core::model::Grid;
use vimj_core::transition::{DeckFx, Timing, Transition};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

/// Automation is recomputed this often, in frames.
const BLOCK: usize = 64;
/// After a synced transition the new song eases back to its own tempo over this many beats.
const GLIDE_BEATS: f64 = 16.0;
/// Decoded songs kept around beyond the ones in use.
const CACHE: usize = 4;
/// "Previous" restarts the song instead when it has played longer than this.
const RESTART_MS: f64 = 3_000.0;
/// A preview keeps playing this long after the window before looping.
const PREVIEW_TAIL_MS: f64 = 3_000.0;
/// Songs are brought to about this loudness.
pub const TARGET_LUFS: f32 = -10.0;

pub struct Entry {
    pub id: String,
    pub path: PathBuf,
    pub grid: Option<Grid>,
    /// Loudness normalisation, dB.
    pub gain_db: f32,
    /// How this song hands over to the next one; `None` plays them back to back.
    pub next: Option<Transition>,
}

/// Loudness normalisation gain for a song, dB.
pub fn normalising_gain(loudness: Option<f32>) -> f32 {
    loudness.map_or(0.0, |l| (TARGET_LUFS - l).clamp(-12.0, 6.0))
}

#[derive(Default)]
pub struct Plan {
    pub entries: Vec<Entry>,
    /// Loop the first transition, starting this many ms before its window.
    pub preview: Option<f64>,
}

pub enum Command {
    /// Replaces the set. The song playing keeps playing if it is in the new one.
    SetPlan(Plan),
    Play { index: usize, position_ms: f64 },
    Pause,
    Resume,
    Stop,
    Seek(f64),
    Next,
    Previous,
    /// 0..=1.
    Volume(f32),
    Loaded { id: String, pcm: Arc<Pcm> },
    LoadFailed { id: String, error: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    Load { id: String, path: PathBuf },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum State {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub state: State,
    pub index: Option<usize>,
    pub position_ms: f64,
    pub tempo: f64,
    /// Index, position and progress through the window of the song coming in.
    pub incoming: Option<(usize, f64, f32)>,
    pub volume: f32,
    /// The last song that could not be loaded, and why.
    pub error: Option<(String, String)>,
}

struct Slot {
    index: usize,
    deck: Deck,
}

struct Active {
    transition: Transition,
    elapsed: f64,
    total: f64,
}

struct Glide {
    from: f64,
    elapsed: f64,
    total: f64,
}

pub struct Engine {
    rate: u32,
    plan: Plan,
    main: Option<Slot>,
    incoming: Option<Slot>,
    active: Option<Active>,
    glide: Option<Glide>,
    /// Decks fading out or ringing with echo.
    tails: Vec<Deck>,
    /// Waiting for this entry to load, to start it at this position.
    pending: Option<(usize, f64)>,
    /// The main song was moved past its outgoing window, so it plays out and cuts.
    skip_window: bool,
    paused: bool,
    fade: Smooth,
    volume: f32,
    master: Smooth,
    cache: Vec<(String, Arc<Pcm>)>,
    requested: HashSet<String>,
    failed: HashSet<String>,
    requests: Vec<Request>,
    error: Option<(String, String)>,
}

impl Engine {
    pub fn new(rate: u32) -> Engine {
        Engine {
            rate,
            plan: Plan::default(),
            main: None,
            incoming: None,
            active: None,
            glide: None,
            tails: Vec::new(),
            pending: None,
            skip_window: false,
            paused: false,
            fade: Smooth::new(0.0, 0.004, rate),
            volume: 0.8,
            master: Smooth::new(0.8, 0.02, rate),
            cache: Vec::new(),
            requested: HashSet::new(),
            failed: HashSet::new(),
            requests: Vec::new(),
            error: None,
        }
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    pub fn command(&mut self, command: Command) {
        match command {
            Command::SetPlan(plan) => self.set_plan(plan),
            Command::Play { index, position_ms } => {
                self.paused = false;
                self.start(index, position_ms);
            }
            Command::Pause => self.paused = true,
            Command::Resume => self.paused = false,
            Command::Stop => {
                self.clear();
                self.pending = None;
            }
            Command::Seek(ms) => {
                if let Some(index) = self.main.as_ref().map(|m| m.index).or(self.pending.map(|p| p.0)) {
                    self.start(index, ms);
                }
            }
            Command::Next => {
                if let Some(index) = self.current_index()
                    && index + 1 < self.plan.entries.len() {
                        self.start(index + 1, self.entry_start_ms(index + 1));
                    }
            }
            Command::Previous => {
                if let Some(index) = self.current_index() {
                    let start = self.entry_start_ms(index);
                    let played = self.main.as_ref().map_or(0.0, |m| self.ms(m.deck.position()) - start);
                    let target = if played > RESTART_MS || index == 0 { index } else { index - 1 };
                    self.start(target, self.entry_start_ms(target));
                }
            }
            Command::Volume(v) => self.volume = v.clamp(0.0, 1.0),
            Command::Loaded { id, pcm } => {
                self.requested.remove(&id);
                self.cache.retain(|(k, _)| *k != id);
                self.cache.push((id, pcm));
                self.evict();
            }
            Command::LoadFailed { id, error } => {
                self.requested.remove(&id);
                self.failed.insert(id.clone());
                if let Some((index, _)) = self.pending
                    && self.plan.entries.get(index).is_some_and(|e| e.id == id)
                {
                    self.pending = None;
                    if index + 1 < self.plan.entries.len() {
                        self.start(index + 1, 0.0);
                    }
                }
                self.error = Some((id, error));
            }
        }
    }

    pub fn snapshot(&self, latency_frames: usize) -> Snapshot {
        let latency = latency_frames as f64;
        let state = if self.main.is_some() {
            if self.paused { State::Paused } else { State::Playing }
        } else if self.pending.is_some() {
            State::Loading
        } else {
            State::Stopped
        };
        let main = self.main.as_ref();
        let incoming = match (&self.incoming, &self.active) {
            (Some(slot), Some(active)) => {
                let behind = if self.paused { 0.0 } else { latency };
                let position = self.ms((slot.deck.position() - behind * slot.deck.tempo()).max(0.0));
                let progress = ((active.elapsed - behind) / active.total).clamp(0.0, 1.0) as f32;
                Some((slot.index, position, progress))
            }
            _ => None,
        };
        Snapshot {
            state,
            index: main.map(|m| m.index).or(self.pending.map(|p| p.0)),
            position_ms: main.map_or_else(
                || self.pending.map_or(0.0, |p| p.1),
                |m| {
                    let behind = if self.paused { 0.0 } else { latency * m.deck.tempo() };
                    self.ms((m.deck.position() - behind).max(0.0))
                },
            ),
            tempo: main.map_or(1.0, |m| m.deck.tempo()),
            incoming,
            volume: self.volume,
            error: self.error.clone(),
        }
    }

    /// Renders interleaved stereo into `out`.
    pub fn render(&mut self, out: &mut [f32]) {
        out.fill(0.0);
        self.start_pending();
        let audible = !self.paused && self.main.is_some();
        if !audible && self.fade.value() < 1e-4 && self.tails.is_empty() {
            self.fade.set(0.0);
            return;
        }
        let frames = out.len() / 2;
        let mut done = 0;
        while done < frames {
            let mut chunk = (frames - done).min(BLOCK);
            if self.active.is_none()
                && let Some(until) = self.frames_to_window()
            {
                if until <= 0.0 {
                    self.begin_transition();
                } else if until < chunk as f64 {
                    chunk = (until.ceil() as usize).max(1);
                }
            }
            self.automate();
            let slice = &mut out[done * 2..(done + chunk) * 2];
            if audible || self.fade.value() >= 1e-4 {
                if let Some(m) = self.main.as_mut() {
                    m.deck.render(slice);
                }
                if let Some(i) = self.incoming.as_mut() {
                    i.deck.render(slice);
                }
            }
            for tail in &mut self.tails {
                tail.render(slice);
            }
            self.tails.retain(|t| !t.silent());
            self.advance(chunk as f64);
            done += chunk;
        }
        let target = if audible { 1.0 } else { 0.0 };
        let volume = self.volume * self.volume;
        for frame in out.as_chunks_mut::<2>().0 {
            let g = self.fade.next(target) * self.master.next(volume);
            frame[0] = soft_clip(frame[0] * g);
            frame[1] = soft_clip(frame[1] * g);
        }
    }

    fn ms(&self, frames: f64) -> f64 {
        frames * 1000.0 / f64::from(self.rate)
    }

    fn frames(&self, ms: f64) -> f64 {
        ms * f64::from(self.rate) / 1000.0
    }

    fn current_index(&self) -> Option<usize> {
        self.main.as_ref().map(|m| m.index).or(self.pending.map(|p| p.0))
    }

    /// Where an entry starts when reached by its incoming transition or skipped to.
    fn entry_start_ms(&self, index: usize) -> f64 {
        index.checked_sub(1).and_then(|i| self.plan.entries[i].next.as_ref()).map_or(0.0, |t| t.in_ms)
    }

    fn timing(&self, index: usize) -> Option<(&Transition, Timing)> {
        let entry = self.plan.entries.get(index)?;
        let next = self.plan.entries.get(index + 1)?;
        let t = entry.next.as_ref()?;
        Some((t, t.timing(entry.grid.as_ref(), next.grid.as_ref())))
    }

    fn level(&self, index: usize) -> f32 {
        10f32.powf(self.plan.entries[index].gain_db / 20.0)
    }

    fn pcm(&mut self, index: usize) -> Option<Arc<Pcm>> {
        let entry = self.plan.entries.get(index)?;
        if let Some(at) = self.cache.iter().position(|(id, _)| *id == entry.id) {
            // Most recently used last.
            let hit = self.cache.remove(at);
            let pcm = hit.1.clone();
            self.cache.push(hit);
            return Some(pcm);
        }
        if !self.failed.contains(&entry.id) && self.requested.insert(entry.id.clone()) {
            self.requests.push(Request::Load { id: entry.id.clone(), path: entry.path.clone() });
        }
        None
    }

    fn evict(&mut self) {
        let mut keep: HashSet<String> = HashSet::new();
        for index in [self.main.as_ref().map(|m| m.index), self.incoming.as_ref().map(|i| i.index), self.pending.map(|p| p.0)]
            .into_iter()
            .flatten()
        {
            for i in [index, index + 1] {
                if let Some(e) = self.plan.entries.get(i) {
                    keep.insert(e.id.clone());
                }
            }
        }
        if self.plan.preview.is_some() {
            keep.extend(self.plan.entries.iter().map(|e| e.id.clone()));
        }
        while self.cache.len() > CACHE.max(keep.len()) {
            match self.cache.iter().position(|(id, _)| !keep.contains(id)) {
                Some(at) => {
                    self.cache.remove(at);
                }
                None => break,
            }
        }
    }

    /// Drops everything playing, fading it out.
    fn clear(&mut self) {
        for slot in [self.main.take(), self.incoming.take()].into_iter().flatten() {
            let mut deck = slot.deck;
            deck.retire();
            self.tails.push(deck);
        }
        self.active = None;
        self.glide = None;
    }

    /// Starts entry `index` at `position_ms`, now or once it has loaded.
    fn start(&mut self, index: usize, position_ms: f64) {
        self.clear();
        if index >= self.plan.entries.len() {
            self.pending = None;
            return;
        }
        self.pending = Some((index, position_ms.max(0.0)));
        self.start_pending();
        // Ask for the next song early, so the transition can start on time.
        let _ = self.pcm(index + 1);
    }

    fn start_pending(&mut self) {
        let Some((index, position_ms)) = self.pending else { return };
        let Some(pcm) = self.pcm(index) else { return };
        self.pending = None;
        let level = self.level(index);
        let deck = Deck::new(pcm, self.frames(position_ms), 1.0, level);
        self.skip_window = self.timing(index).is_some_and(|(_, t)| position_ms >= t.out_end_ms);
        self.main = Some(Slot { index, deck });
        let _ = self.pcm(index + 1);
        self.evict();
    }

    /// Real frames until the main song reaches its outgoing window.
    fn frames_to_window(&self) -> Option<f64> {
        let main = self.main.as_ref()?;
        if self.skip_window || self.paused {
            return None;
        }
        let (_, timing) = self.timing(main.index)?;
        Some((self.frames(timing.out_start_ms) - main.deck.position()) / main.deck.tempo())
    }

    fn begin_transition(&mut self) {
        let Some(main) = self.main.as_ref() else { return };
        let index = main.index;
        let Some((transition, timing)) = self.timing(index).map(|(t, tm)| (t.clone(), tm)) else { return };
        let a_tempo = main.deck.tempo();
        let position_ms = self.ms(main.deck.position());
        // Wall-clock time already into the window, when starting late.
        let elapsed_ms = ((position_ms - timing.out_start_ms) / a_tempo).max(0.0);
        let total_ms = timing.duration_ms / a_tempo;
        if elapsed_ms >= total_ms {
            self.skip_window = true;
            return;
        }
        let Some(pcm) = self.pcm(index + 1) else { return };
        let b_tempo = timing.rate * a_tempo;
        let b_position = self.frames(timing.in_start_ms + elapsed_ms * b_tempo);
        let mut deck = Deck::new(pcm, b_position, b_tempo, self.level(index + 1));
        let point = transition.at((elapsed_ms / total_ms) as f32);
        deck.fx = point.inc;
        let main = self.main.as_mut().expect("checked above");
        main.deck.fx = point.out;
        self.incoming = Some(Slot { index: index + 1, deck });
        self.glide = None;
        self.active = Some(Active { transition, elapsed: self.frames(elapsed_ms), total: self.frames(total_ms) });
        self.arm_outgoing();
        let _ = self.pcm(index + 2);
    }

    /// Gives the outgoing deck the effect and loop of the transition under way.
    fn arm_outgoing(&mut self) {
        let (Some(active), Some(main)) = (&self.active, self.main.as_mut()) else { return };
        let grid = self.plan.entries[main.index].grid;
        let per_ms = f64::from(self.rate) / 1000.0;
        // Echoes repeat in beats as heard, at the deck's tempo.
        let beat_ms = grid.map_or(500.0, |g| g.beat_ms()) / main.deck.tempo();
        main.deck.set_effect(active.transition.effect.kind(), beat_ms * per_ms);
        main.deck.set_loop(active.transition.loop_map(grid.as_ref()).map(|m| m.scaled(per_ms)));
    }

    fn automate(&mut self) {
        if let Some(active) = &self.active {
            let point = active.transition.at((active.elapsed / active.total) as f32);
            if let Some(m) = self.main.as_mut() {
                m.deck.fx = point.out;
            }
            if let Some(i) = self.incoming.as_mut() {
                i.deck.fx = point.inc;
            }
        } else if let Some(glide) = &self.glide
            && let Some(m) = self.main.as_mut()
        {
            let x = (glide.elapsed / glide.total).clamp(0.0, 1.0);
            let eased = x * x * (3.0 - 2.0 * x);
            m.deck.set_tempo(glide.from + (1.0 - glide.from) * eased);
        }
    }

    fn advance(&mut self, frames: f64) {
        if let Some(active) = self.active.as_mut() {
            active.elapsed += frames;
            if active.elapsed >= active.total {
                self.finish_transition();
            }
            return;
        }
        if let Some(glide) = self.glide.as_mut() {
            glide.elapsed += frames;
            if glide.elapsed >= glide.total {
                self.glide = None;
                if let Some(m) = self.main.as_mut() {
                    m.deck.set_tempo(1.0);
                }
            }
        }
        if let Some(lead) = self.plan.preview
            && let Some(m) = &self.main
            && m.index == 1
            && let Some((_, timing)) = self.timing(0)
            && (self.ms(m.deck.position()) >= timing.in_end_ms + PREVIEW_TAIL_MS || m.deck.at_end())
        {
            self.start(0, timing.out_start_ms - lead);
            return;
        }
        if self.main.as_ref().is_some_and(|m| m.deck.at_end()) {
            self.song_ended();
        }
    }

    fn finish_transition(&mut self) {
        let Some(active) = self.active.take() else { return };
        if let Some(mut old) = self.main.take() {
            old.deck.retire();
            if !active.transition.effect.rings_out() {
                old.deck.cut_tail();
            }
            self.tails.push(old.deck);
        }
        let Some(mut new) = self.incoming.take() else { return };
        new.deck.fx = DeckFx::FLAT;
        let tempo = new.deck.tempo();
        if (tempo - 1.0).abs() > 1e-6 {
            let beat_ms = self.plan.entries[new.index].grid.map_or(500.0, |g| g.beat_ms());
            self.glide = Some(Glide { from: tempo, elapsed: 0.0, total: self.frames(beat_ms * GLIDE_BEATS / tempo) });
        }
        self.skip_window = false;
        let index = new.index;
        self.main = Some(new);
        let _ = self.pcm(index + 1);
        self.evict();
    }

    /// The main song ran out without a transition.
    fn song_ended(&mut self) {
        let Some(main) = self.main.as_ref() else { return };
        let index = main.index;
        if self.plan.preview.is_some() {
            if let Some((_, timing)) = self.timing(0) {
                let lead = self.plan.preview.unwrap_or(0.0);
                self.start(0, timing.out_start_ms - lead);
            }
            return;
        }
        if index + 1 >= self.plan.entries.len() {
            self.clear();
            return;
        }
        // Skipped windows resume the next song where the window would have left it.
        let position = match (self.skip_window, self.timing(index)) {
            (true, Some((_, t))) => t.in_end_ms,
            _ => 0.0,
        };
        self.start(index + 1, position);
    }

    fn set_plan(&mut self, plan: Plan) {
        let playing = self.main.as_ref().map(|m| (m.index, self.plan.entries[m.index].id.clone()));
        let coming = self.incoming.as_ref().map(|i| self.plan.entries[i.index].id.clone());
        let pending = self.pending.map(|(index, pos)| (self.plan.entries[index].id.clone(), pos));
        self.plan = plan;
        // Find the playing song in the new set, nearest its old place.
        let find = |plan: &Plan, id: &str, near: usize| {
            plan.entries.iter().enumerate().filter(|(_, e)| e.id == id).map(|(i, _)| i).min_by_key(|&i| i.abs_diff(near))
        };
        if let Some((pending_id, position)) = pending {
            self.pending = find(&self.plan, &pending_id, 0).map(|i| (i, position));
        }
        let Some((old_index, id)) = playing else { return };
        let Some(index) = find(&self.plan, &id, old_index) else {
            // Gone from the set: let it finish, then stop.
            self.clear();
            return;
        };
        if let Some(m) = self.main.as_mut() {
            m.index = index;
        }
        let next_id = self.plan.entries.get(index + 1).map(|e| e.id.clone());
        match (&mut self.active, coming) {
            (Some(active), Some(coming)) if next_id.as_deref() == Some(&coming) => {
                // Same pair: keep going, with the new styles and curves.
                if let Some(t) = &self.plan.entries[index].next {
                    active.transition = t.clone();
                }
                if let Some(i) = self.incoming.as_mut() {
                    i.index = index + 1;
                }
                self.arm_outgoing();
            }
            (Some(_), _) => {
                self.active = None;
                if let Some(mut i) = self.incoming.take() {
                    i.deck.retire();
                    self.tails.push(i.deck);
                }
                if let Some(m) = self.main.as_mut() {
                    m.deck.fx = DeckFx::FLAT;
                }
            }
            _ => {}
        }
        let position_ms = self.main.as_ref().map_or(0.0, |m| self.ms(m.deck.position()));
        self.skip_window = self.active.is_none() && self.timing(index).is_some_and(|(_, t)| position_ms >= t.out_start_ms);
        let _ = self.pcm(index + 1);
        self.evict();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vimj_core::transition::{Curves, Effect, Eq, Filter, Looping, Preset, Volume};

    const RATE: u32 = 48_000;

    /// A tone of `hz` with a short burst of `click_hz` on every beat.
    fn song(seconds: f64, bpm: f64, hz: f32, click_hz: f32) -> Arc<Pcm> {
        let frames = (seconds * f64::from(RATE)) as usize;
        let beat = 60.0 / bpm * f64::from(RATE);
        let mut s = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let t = i as f32 / RATE as f32;
            let in_beat = (i as f64 % beat) as f32;
            let click = if in_beat < 480.0 { (2.0 * std::f32::consts::PI * click_hz * t).sin() * 0.4 } else { 0.0 };
            let v = 0.2 * (2.0 * std::f32::consts::PI * hz * t).sin() + click;
            s.push(v);
            s.push(v);
        }
        Arc::new(Pcm::from_f32(RATE, &s))
    }

    fn entry(id: &str, bpm: f64, next: Option<Transition>) -> Entry {
        Entry {
            id: id.into(),
            path: PathBuf::from(format!("{id}.opus")),
            grid: Some(Grid { bpm, downbeat_ms: 0.0 }),
            gain_db: 0.0,
            next,
        }
    }

    fn blend(out_ms: f64, in_ms: f64, bars: u32) -> Transition {
        let mut t = Transition {
            volume: Volume::Smooth,
            eq: Eq::Off,
            filter: Filter::None,
            effect: Effect::None,
            looping: Looping::None,
            curves: Curves::default(),
            bars,
            out_ms,
            in_ms,
            sync: true,
            auto: false,
        };
        t.set_preset(Preset::Fade);
        t
    }

    /// Engine with both songs loaded and the plan set.
    fn engine(plan: Plan, songs: &[(&str, Arc<Pcm>)]) -> Engine {
        let mut e = Engine::new(RATE);
        e.command(Command::Volume(1.0));
        e.command(Command::SetPlan(plan));
        for (id, pcm) in songs {
            e.command(Command::Loaded { id: (*id).into(), pcm: pcm.clone() });
        }
        e
    }

    fn run(e: &mut Engine, seconds: f64) -> Vec<f32> {
        let frames = (seconds * f64::from(RATE)) as usize;
        let mut out = vec![0.0; frames * 2];
        for block in out.chunks_mut(1_024) {
            e.render(block);
        }
        out.as_chunks::<2>().0.iter().map(|f| f[0]).collect()
    }

    /// Goertzel amplitude of `hz` over windows of `n` frames.
    fn goertzel(signal: &[f32], hz: f32, n: usize) -> Vec<f32> {
        let coeff = 2.0 * (2.0 * std::f32::consts::PI * hz / RATE as f32).cos();
        signal
            .chunks(n)
            .map(|w| {
                let (mut s1, mut s2) = (0.0f32, 0.0f32);
                for &x in w {
                    let s = x + coeff * s1 - s2;
                    s2 = s1;
                    s1 = s;
                }
                (s1 * s1 + s2 * s2 - coeff * s1 * s2).sqrt() / n as f32
            })
            .collect()
    }

    /// Level of a steady tone, per 100 ms.
    fn presence(signal: &[f32], hz: f32) -> Vec<f32> {
        goertzel(signal, hz, 4_800)
    }

    /// Times (10 ms windows) where a click of `hz` starts.
    fn click_windows(signal: &[f32], hz: f32) -> Vec<usize> {
        let p = goertzel(signal, hz, 480);
        let peak = p.iter().fold(0.0f32, |m, &x| m.max(x));
        let mut starts = Vec::new();
        for i in 1..p.len() {
            if p[i] > peak * 0.3 && p[i - 1] <= peak * 0.3 {
                starts.push(i);
            }
        }
        starts
    }

    #[test]
    fn mixes_one_song_into_the_next_on_the_beat() {
        let a = song(40.0, 120.0, 440.0, 1_000.0);
        let b = song(40.0, 125.0, 660.0, 5_000.0);
        // A leaves at 20 s over 4 bars (8 s); B enters at 2.4 s (5 beats in).
        let t = blend(20_000.0, 2_400.0, 4);
        let plan = Plan { entries: vec![entry("a", 120.0, Some(t)), entry("b", 125.0, None)], preview: None };
        let mut e = engine(plan, &[("a", a), ("b", b)]);
        e.command(Command::Play { index: 0, position_ms: 10_000.0 });
        let out = run(&mut e, 25.0);

        let a_tone = presence(&out, 440.0);
        let b_tone = presence(&out, 660.0);
        // Window: 10 s..18 s of output. B silent before, A silent after.
        assert!(b_tone[90] < 0.005 && a_tone[90] > 0.05, "only A before the window");
        assert!(b_tone[140] > 0.03 && a_tone[140] > 0.03, "both in the middle");
        assert!(a_tone[185] < 0.005 && b_tone[185] > 0.05, "only B after the window");
        // A plays from a beat, so its beats fall on every 500 ms of output.
        let a_clicks = click_windows(&out[..14 * RATE as usize], 1_000.0);
        assert!(a_clicks.iter().all(|w| w % 50 == 0), "{a_clicks:?}");
        // While both play, B's clicks sit on A's beats.
        let b_clicks: Vec<usize> = click_windows(&out, 5_000.0).into_iter().filter(|&w| (1_000..1_800).contains(&w)).collect();
        assert!(b_clicks.len() >= 10, "{b_clicks:?}");
        for w in &b_clicks {
            assert!(w % 50 <= 1 || w % 50 == 49, "B click at {w}0 ms is off A's beat");
        }
        assert!(out.iter().all(|x| x.abs() <= 1.0));
        let snap = e.snapshot(0);
        assert_eq!(snap.index, Some(1));
        assert!(snap.tempo < 1.0, "still easing back from A's tempo");
        // After the window B eases back to its own tempo over 16 beats (8 s).
        run(&mut e, 2.0);
        let snap = e.snapshot(0);
        assert_eq!(snap.tempo, 1.0);
        // 2.4 s in + 16 beats at 125 BPM by 18 s, then 9 s more, the first 8 a little slower.
        let expected = 2_400.0 + 16.0 * 480.0 + 9_000.0 - 8_000.0 * 0.02;
        assert!((snap.position_ms - expected).abs() < 200.0, "{} vs {expected}", snap.position_ms);
    }

    #[test]
    fn plays_back_to_back_without_a_transition() {
        let a = song(2.0, 120.0, 440.0, 1_000.0);
        let b = song(4.0, 120.0, 660.0, 5_000.0);
        let plan = Plan { entries: vec![entry("a", 120.0, None), entry("b", 120.0, None)], preview: None };
        let mut e = engine(plan, &[("a", a), ("b", b)]);
        e.command(Command::Play { index: 0, position_ms: 0.0 });
        let out = run(&mut e, 3.0);
        let b_tone = presence(&out, 660.0);
        assert!(b_tone[19] < 0.005 && b_tone[21] > 0.05, "{:?}", &b_tone[17..23]);
        assert_eq!(e.snapshot(0).index, Some(1));
        run(&mut e, 4.0);
        assert_eq!(e.snapshot(0).state, State::Stopped);
    }

    #[test]
    fn asks_for_songs_and_waits_for_them() {
        let plan = Plan { entries: vec![entry("a", 120.0, None), entry("b", 120.0, None)], preview: None };
        let mut e = Engine::new(RATE);
        e.command(Command::SetPlan(plan));
        e.command(Command::Play { index: 0, position_ms: 1_000.0 });
        let asked = e.take_requests();
        assert_eq!(asked.len(), 2, "the song and the one after: {asked:?}");
        assert_eq!(e.snapshot(0).state, State::Loading);
        assert!(run(&mut e, 0.1).iter().all(|&x| x == 0.0));
        e.command(Command::Loaded { id: "a".into(), pcm: song(5.0, 120.0, 440.0, 1_000.0) });
        run(&mut e, 0.5);
        let s = e.snapshot(0);
        assert_eq!(s.state, State::Playing);
        assert!((s.position_ms - 1_500.0).abs() < 5.0, "{}", s.position_ms);
        assert!(e.take_requests().is_empty(), "no repeated requests");
    }

    #[test]
    fn seeking_into_a_window_starts_the_transition_there() {
        let a = song(40.0, 120.0, 440.0, 1_000.0);
        let b = song(40.0, 120.0, 660.0, 5_000.0);
        let plan = Plan { entries: vec![entry("a", 120.0, Some(blend(20_000.0, 0.0, 4))), entry("b", 120.0, None)], preview: None };
        let mut e = engine(plan, &[("a", a), ("b", b)]);
        e.command(Command::Play { index: 0, position_ms: 24_000.0 });
        run(&mut e, 0.5);
        let s = e.snapshot(0);
        let (index, position, progress) = s.incoming.expect("mixing");
        assert_eq!(index, 1);
        assert!((position - 4_500.0).abs() < 20.0, "{position}");
        assert!((progress - 4.5 / 8.0).abs() < 0.01, "{progress}");
    }

    #[test]
    fn seeking_past_a_window_plays_out_then_cuts() {
        let a = song(30.0, 120.0, 440.0, 1_000.0);
        let b = song(40.0, 120.0, 660.0, 5_000.0);
        let plan = Plan { entries: vec![entry("a", 120.0, Some(blend(20_000.0, 0.0, 4))), entry("b", 120.0, None)], preview: None };
        let mut e = engine(plan, &[("a", a), ("b", b)]);
        e.command(Command::Play { index: 0, position_ms: 29_000.0 });
        run(&mut e, 0.5);
        assert!(e.snapshot(0).incoming.is_none());
        run(&mut e, 1.0);
        let s = e.snapshot(0);
        assert_eq!(s.index, Some(1));
        // Where the window would have left B: 8 s in.
        assert!((s.position_ms - 8_500.0).abs() < 30.0, "{}", s.position_ms);
    }

    #[test]
    fn preview_loops_the_transition() {
        let a = song(40.0, 120.0, 440.0, 1_000.0);
        let b = song(40.0, 120.0, 660.0, 5_000.0);
        let plan = Plan { entries: vec![entry("a", 120.0, Some(blend(20_000.0, 0.0, 2))), entry("b", 120.0, None)], preview: Some(2_000.0) };
        let mut e = engine(plan, &[("a", a), ("b", b)]);
        e.command(Command::Play { index: 0, position_ms: 18_000.0 });
        // 2 s lead, 4 s window, 3 s tail, then back to A at 18 s.
        run(&mut e, 8.5);
        assert_eq!(e.snapshot(0).index, Some(1));
        run(&mut e, 1.0);
        let s = e.snapshot(0);
        assert_eq!(s.index, Some(0));
        assert!((s.position_ms - 18_500.0).abs() < 30.0, "{}", s.position_ms);
    }

    #[test]
    fn next_and_previous_follow_the_entry_points() {
        let a = song(30.0, 120.0, 440.0, 1_000.0);
        let b = song(30.0, 120.0, 660.0, 5_000.0);
        let plan = Plan { entries: vec![entry("a", 120.0, Some(blend(20_000.0, 6_000.0, 4))), entry("b", 120.0, None)], preview: None };
        let mut e = engine(plan, &[("a", a), ("b", b)]);
        e.command(Command::Play { index: 0, position_ms: 5_000.0 });
        e.command(Command::Next);
        run(&mut e, 1.0);
        let s = e.snapshot(0);
        assert_eq!(s.index, Some(1));
        assert!((s.position_ms - 7_000.0).abs() < 20.0, "{}", s.position_ms);
        e.command(Command::Previous);
        run(&mut e, 0.1);
        // Played under 3 s from its entry point, so back to the previous song.
        assert_eq!(e.snapshot(0).index, Some(0));
    }

    #[test]
    fn pausing_freezes_the_position() {
        let plan = Plan { entries: vec![entry("a", 120.0, None)], preview: None };
        let mut e = engine(plan, &[("a", song(10.0, 120.0, 440.0, 1_000.0))]);
        e.command(Command::Play { index: 0, position_ms: 0.0 });
        run(&mut e, 1.0);
        e.command(Command::Pause);
        run(&mut e, 0.1);
        let paused = e.snapshot(0).position_ms;
        let quiet = run(&mut e, 1.0);
        assert!(quiet.iter().all(|&x| x == 0.0));
        assert_eq!(e.snapshot(0).position_ms, paused);
        assert_eq!(e.snapshot(0).state, State::Paused);
        e.command(Command::Resume);
        run(&mut e, 1.0);
        assert!((e.snapshot(0).position_ms - paused - 1_000.0).abs() < 5.0);
    }

    /// A tone that changes every bar: bar `k` at `1 000 + 100 k` Hz.
    fn stepped(seconds: f64, bpm: f64) -> Arc<Pcm> {
        let bar = 4.0 * 60.0 / bpm;
        let frames = (seconds * f64::from(RATE)) as usize;
        let mut s = Vec::with_capacity(frames * 2);
        for i in 0..frames {
            let t = i as f64 / f64::from(RATE);
            let hz = 1_000.0 + 100.0 * (t / bar).floor();
            let v = 0.3 * (2.0 * std::f64::consts::PI * hz * t).sin() as f32;
            s.push(v);
            s.push(v);
        }
        Arc::new(Pcm::from_f32(RATE, &s))
    }

    #[test]
    fn loops_the_first_bar_of_the_window() {
        let a = stepped(40.0, 120.0);
        let b = song(40.0, 120.0, 440.0, 5_000.0);
        // A leaves at bar 10 (20 s, 2 000 Hz) over 4 bars, looping its first.
        let mut t = blend(20_000.0, 0.0, 4);
        t.set_volume(Volume::FadeInCutOut);
        t.looping = Looping::FourBeat;
        let plan = Plan { entries: vec![entry("a", 120.0, Some(t)), entry("b", 120.0, None)], preview: None };
        let mut e = engine(plan, &[("a", a), ("b", b)]);
        e.command(Command::Play { index: 0, position_ms: 10_000.0 });
        let out = run(&mut e, 18.0);
        let bar = |k: f32| presence(&out, 1_000.0 + 100.0 * k);
        // Before the window A moves on bar by bar; in it, bar 10 comes round again and again.
        assert!(bar(9.0)[90] > 0.05, "bar 9 at 9 s");
        for at in [110, 130, 150, 170] {
            assert!(bar(10.0)[at] > 0.05, "bar 10 at {at}00 ms");
            assert!(bar(11.0)[at] < 0.01 && bar(12.0)[at] < 0.01 && bar(13.0)[at] < 0.01, "later bars at {at}00 ms");
        }
    }

    #[test]
    fn effects_ring_on_after_the_window_when_out_and_stop_when_cut() {
        let echo_after = |effect: Effect| {
            let a = song(40.0, 120.0, 440.0, 1_000.0);
            let b = song(40.0, 120.0, 660.0, 5_000.0);
            let mut t = blend(20_000.0, 0.0, 2);
            t.set_volume(Volume::FadeInCutOut);
            t.set_effect(effect);
            let plan = Plan { entries: vec![entry("a", 120.0, Some(t)), entry("b", 120.0, None)], preview: None };
            let mut e = engine(plan, &[("a", a), ("b", b)]);
            e.command(Command::Play { index: 0, position_ms: 18_000.0 });
            // The window runs 2 s..6 s.
            let out = run(&mut e, 7.0);
            (presence(&out, 440.0)[50], presence(&out, 440.0)[63])
        };
        let (during, after) = echo_after(Effect::EchoOneOutEnd);
        assert!(during > 0.05 && after > 0.02, "{during} {after}");
        let (during, after) = echo_after(Effect::EchoOneCutEnd);
        assert!(during > 0.05 && after < 0.002, "{during} {after}");
        let (_, after) = echo_after(Effect::ReverbOutEnd);
        assert!(after > 0.002, "{after}");
        let (_, after) = echo_after(Effect::ReverbCutEnd);
        assert!(after < 0.0002, "{after}");
    }

    #[test]
    fn replanning_keeps_the_song_playing() {
        let a = song(30.0, 120.0, 440.0, 1_000.0);
        let b = song(30.0, 120.0, 660.0, 5_000.0);
        let plan = Plan { entries: vec![entry("a", 120.0, None), entry("b", 120.0, None)], preview: None };
        let mut e = engine(plan, &[("a", a), ("b", b)]);
        e.command(Command::Play { index: 1, position_ms: 0.0 });
        run(&mut e, 1.0);
        e.command(Command::SetPlan(Plan { entries: vec![entry("b", 120.0, None), entry("a", 120.0, None)], preview: None }));
        let s = e.snapshot(0);
        assert_eq!(s.index, Some(0));
        assert!((s.position_ms - 1_000.0).abs() < 5.0);
    }
}
