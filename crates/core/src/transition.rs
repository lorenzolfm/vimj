//! Transitions: how one song hands over to the next.
//!
//! Both songs play together for a whole number of bars of the outgoing song.
//! Across that window three curves shape the handover, as in Spotify's Mix:
//! volume, EQ (low/mid/high) and an effect (filter sweeps, echo, brake). The
//! incoming song is time-stretched to the outgoing tempo while they overlap,
//! so their beats stay locked, then eases back to its own tempo.

use crate::compat::Compat;
use crate::model::{Analysis, BEATS_PER_BAR, Grid};
use serde::{Deserialize, Serialize};

/// Tempo assumed for bar lengths when a song has no beat grid.
pub const FALLBACK_BPM: f64 = 120.0;
/// Largest tempo change applied to line the incoming song up, as a fraction.
pub const MAX_SYNC: f64 = 0.10;
/// The incoming song keeps at least this much after the transition.
pub const MIN_REMAINING_MS: f64 = 5_000.0;
/// Transition lengths offered, in bars of the outgoing song.
pub const BAR_CHOICES: [u32; 5] = [1, 2, 4, 8, 16];
/// Phrase length used to place automatic transitions.
pub const PHRASE_BARS: u32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Preset {
    Fade,
    Rise,
    Blend,
    Wave,
    Melt,
    Slam,
    /// Curves picked by hand.
    Custom,
}

impl Preset {
    pub const ALL: [Preset; 6] = [Preset::Fade, Preset::Rise, Preset::Blend, Preset::Wave, Preset::Melt, Preset::Slam];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Fade => "Fade",
            Preset::Rise => "Rise",
            Preset::Blend => "Blend",
            Preset::Wave => "Wave",
            Preset::Melt => "Melt",
            Preset::Slam => "Slam",
            Preset::Custom => "Custom",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Preset::Fade => "Smooth crossfade, bass swaps at the midpoint",
            Preset::Rise => "Overlap, low-pass in and high-pass out, bass swaps at the end",
            Preset::Blend => "Overlap with a smooth three-band EQ handover",
            Preset::Wave => "Overlap, low-pass sweep on both, bass swaps at the midpoint",
            Preset::Melt => "Crossfade, high-pass sweep on both, bass swaps at the midpoint",
            Preset::Slam => "Hard volume swap at the midpoint",
            Preset::Custom => "Your own volume, EQ and effect curves",
        }
    }

    fn curves(self) -> Option<(Volume, Eq, Effect)> {
        Some(match self {
            Preset::Fade => (Volume::Smooth, Eq::CenterBass, Effect::None),
            Preset::Rise => (Volume::Overlap, Eq::EndBass, Effect::FilterSwap),
            Preset::Blend => (Volume::Overlap, Eq::ThreeBand, Effect::None),
            Preset::Wave => (Volume::Overlap, Eq::CenterBass, Effect::LowPassBoth),
            Preset::Melt => (Volume::Crossfade, Eq::CenterBass, Effect::HighPassBoth),
            Preset::Slam => (Volume::CenterCut, Eq::Off, Effect::None),
            Preset::Custom => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Volume {
    Smooth,
    Crossfade,
    Overlap,
    FadeInFadeOut,
    CutInFadeOut,
    FadeInCutOut,
    CenterCut,
}

impl Volume {
    pub const ALL: [Volume; 7] = [
        Volume::Smooth,
        Volume::Crossfade,
        Volume::Overlap,
        Volume::FadeInFadeOut,
        Volume::CutInFadeOut,
        Volume::FadeInCutOut,
        Volume::CenterCut,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Volume::Smooth => "Smooth crossfade",
            Volume::Crossfade => "Crossfade",
            Volume::Overlap => "Overlap",
            Volume::FadeInFadeOut => "Fade in, fade out",
            Volume::CutInFadeOut => "Cut in, fade out",
            Volume::FadeInCutOut => "Fade in, cut out",
            Volume::CenterCut => "Center cut",
        }
    }

    /// Gains of the outgoing and incoming song at progress `x`.
    fn gains(self, x: f32) -> (f32, f32) {
        match self {
            Volume::Smooth => {
                let angle = x.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
                (angle.cos(), angle.sin())
            }
            Volume::Crossfade => (1.0 - x.clamp(0.0, 1.0), x.clamp(0.0, 1.0)),
            Volume::Overlap => (1.0 - ramp(x, 0.85, 1.0), ramp(x, 0.0, 0.15)),
            Volume::FadeInFadeOut => (1.0 - ramp(x, 0.5, 1.0), ramp(x, 0.0, 0.5)),
            Volume::CutInFadeOut => (1.0 - ramp(x, 0.0, 1.0), 1.0),
            Volume::FadeInCutOut => (1.0 - ramp(x, 0.97, 1.0), ramp(x, 0.0, 1.0)),
            Volume::CenterCut => (1.0 - step(x, 0.5), step(x, 0.5)),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Eq {
    CenterBass,
    EndBass,
    ThreeBand,
    Off,
}

impl Eq {
    pub const ALL: [Eq; 4] = [Eq::CenterBass, Eq::EndBass, Eq::ThreeBand, Eq::Off];

    pub fn label(self) -> &'static str {
        match self {
            Eq::CenterBass => "Center bass swap",
            Eq::EndBass => "End bass swap",
            Eq::ThreeBand => "3-band fade",
            Eq::Off => "None",
        }
    }

    /// Low/mid/high gains of the outgoing and incoming song at progress `x`.
    fn bands(self, x: f32) -> ([f32; 3], [f32; 3]) {
        match self {
            Eq::CenterBass => {
                let s = step(x, 0.5);
                ([1.0 - s, 1.0, 1.0], [s, 1.0, 1.0])
            }
            Eq::EndBass => {
                let s = step(x, 0.9);
                ([1.0 - s, 1.0, 1.0], [s, 1.0, 1.0])
            }
            Eq::ThreeBand => {
                let (low, mid, high) = (ramp(x, 0.6, 1.0), ramp(x, 0.3, 0.7), ramp(x, 0.0, 0.4));
                ([1.0 - low, 1.0 - mid, 1.0 - high], [low, mid, high])
            }
            Eq::Off => ([1.0; 3], [1.0; 3]),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    None,
    HighPassOut,
    LowPassIn,
    FilterSwap,
    LowPassBoth,
    HighPassBoth,
    Echo,
    FilterEcho,
    Brake,
}

/// How far the filter knob turns during sweeps.
const SWEEP: f32 = 0.8;

impl Effect {
    pub const ALL: [Effect; 9] = [
        Effect::None,
        Effect::HighPassOut,
        Effect::LowPassIn,
        Effect::FilterSwap,
        Effect::LowPassBoth,
        Effect::HighPassBoth,
        Effect::Echo,
        Effect::FilterEcho,
        Effect::Brake,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Effect::None => "None",
            Effect::HighPassOut => "High-pass out",
            Effect::LowPassIn => "Low-pass in",
            Effect::FilterSwap => "Filter swap",
            Effect::LowPassBoth => "Low-pass sweep",
            Effect::HighPassBoth => "High-pass sweep",
            Effect::Echo => "Echo out",
            Effect::FilterEcho => "Filter echo out",
            Effect::Brake => "Brake",
        }
    }

    fn apply(self, x: f32, out: &mut DeckFx, inc: &mut DeckFx) {
        let sweep = ramp(x, 0.0, 1.0) * SWEEP;
        let open = (1.0 - ramp(x, 0.0, 1.0)) * SWEEP;
        match self {
            Effect::None => {}
            Effect::HighPassOut => out.filter = sweep,
            Effect::LowPassIn => inc.filter = -open,
            Effect::FilterSwap => {
                out.filter = sweep;
                inc.filter = -open;
            }
            Effect::LowPassBoth => {
                out.filter = -sweep;
                inc.filter = -open;
            }
            Effect::HighPassBoth => {
                out.filter = sweep;
                inc.filter = open;
            }
            Effect::Echo => out.echo = ramp(x, 0.5, 0.9),
            Effect::FilterEcho => {
                out.echo = ramp(x, 0.5, 0.9);
                out.filter = sweep;
            }
            Effect::Brake => out.speed = 1.0 - ramp(x, 0.75, 1.0),
        }
    }

    /// Whether the outgoing song keeps sounding after the window ends.
    pub fn has_tail(self) -> bool {
        matches!(self, Effect::Echo | Effect::FilterEcho)
    }
}

/// Settings for one deck at one moment of a transition.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeckFx {
    pub gain: f32,
    /// Low, mid, high: 1 is flat, 0 kills the band.
    pub eq: [f32; 3],
    /// Filter knob: below 0 low-pass, above 0 high-pass, 0 open; ±1 fully closed.
    pub filter: f32,
    /// Echo send, 0..=1.
    pub echo: f32,
    /// Playback speed (varispeed, pitch follows), 1 is normal.
    pub speed: f32,
}

impl DeckFx {
    pub const FLAT: DeckFx = DeckFx { gain: 1.0, eq: [1.0; 3], filter: 0.0, echo: 0.0, speed: 1.0 };
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Point {
    pub out: DeckFx,
    pub inc: DeckFx,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub preset: Preset,
    pub volume: Volume,
    pub eq: Eq,
    pub effect: Effect,
    /// Length in bars of the outgoing song.
    pub bars: u32,
    /// Position in the outgoing song where the transition starts.
    pub out_ms: f64,
    /// Position in the incoming song at that moment.
    pub in_ms: f64,
    /// Stretch the incoming song to the outgoing tempo while they overlap.
    pub sync: bool,
    /// Generated rather than edited by hand.
    pub auto: bool,
}

impl Transition {
    pub fn set_preset(&mut self, preset: Preset) {
        self.preset = preset;
        if let Some((volume, eq, effect)) = preset.curves() {
            self.volume = volume;
            self.eq = eq;
            self.effect = effect;
        }
    }

    /// Records that a curve was changed by hand.
    pub fn customize(&mut self) {
        if Some((self.volume, self.eq, self.effect)) != self.preset.curves() {
            self.preset = Preset::Custom;
        }
    }

    /// Deck settings at progress `x` (0 at the start of the window, 1 at its end).
    pub fn at(&self, x: f32) -> Point {
        let (a, b) = self.volume.gains(x);
        let (a_eq, b_eq) = self.eq.bands(x);
        let mut out = DeckFx { gain: a, eq: a_eq, ..DeckFx::FLAT };
        let mut inc = DeckFx { gain: b, eq: b_eq, ..DeckFx::FLAT };
        self.effect.apply(x, &mut out, &mut inc);
        Point { out, inc }
    }

    pub fn timing(&self, from: Option<&Grid>, to: Option<&Grid>) -> Timing {
        let beat = from.map_or(60_000.0 / FALLBACK_BPM, Grid::beat_ms);
        let duration_ms = f64::from(self.bars * BEATS_PER_BAR) * beat;
        let rate = match (self.sync, from, to) {
            (true, Some(a), Some(b)) => sync_rate(a.bpm, b.bpm).unwrap_or(1.0),
            _ => 1.0,
        };
        Timing {
            out_start_ms: self.out_ms,
            out_end_ms: self.out_ms + duration_ms,
            in_start_ms: self.in_ms,
            in_end_ms: self.in_ms + duration_ms * rate,
            duration_ms,
            rate,
        }
    }

    /// Keeps the window inside both songs, shortening it if a song is too short.
    pub fn clamp(&mut self, from: Side, to: Side) {
        let (a, b) = (from.grid(), to.grid());
        while self.bars > 1 {
            let t = self.timing(a.as_ref(), b.as_ref());
            if t.duration_ms <= from.duration_ms && t.duration_ms * t.rate + MIN_REMAINING_MS <= to.duration_ms {
                break;
            }
            self.bars /= 2;
        }
        let t = self.timing(a.as_ref(), b.as_ref());
        self.out_ms = self.out_ms.clamp(0.0, (from.duration_ms - t.duration_ms).max(0.0));
        let in_max = to.duration_ms - MIN_REMAINING_MS - t.duration_ms * t.rate;
        self.in_ms = self.in_ms.clamp(0.0, in_max.max(0.0));
    }
}

/// Where a transition's window sits in both songs, in milliseconds.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timing {
    pub out_start_ms: f64,
    pub out_end_ms: f64,
    pub in_start_ms: f64,
    pub in_end_ms: f64,
    /// Wall-clock length.
    pub duration_ms: f64,
    /// Playback rate of the incoming song during the window.
    pub rate: f64,
}

/// Playback rate that puts the incoming song's beats on the outgoing tempo,
/// folding half and double time. `None` when that needs too much stretch.
pub fn sync_rate(out_bpm: f64, in_bpm: f64) -> Option<f64> {
    [out_bpm, out_bpm * 2.0, out_bpm / 2.0]
        .into_iter()
        .map(|target| target / in_bpm)
        .min_by(|a, b| (a - 1.0).abs().total_cmp(&(b - 1.0).abs()))
        .filter(|rate| (rate - 1.0).abs() <= MAX_SYNC)
}

/// A song as transitions see it.
#[derive(Clone, Copy, Debug)]
pub struct Side<'a> {
    pub duration_ms: f64,
    pub analysis: Option<&'a Analysis>,
}

impl Side<'_> {
    fn grid(&self) -> Option<Grid> {
        self.analysis.and_then(|a| a.grid)
    }

    fn start_ms(&self) -> f64 {
        self.analysis.map_or(0.0, |a| a.start_ms)
    }

    fn end_ms(&self) -> f64 {
        self.analysis.map_or(self.duration_ms, |a| a.end_ms.min(self.duration_ms))
    }
}

/// Picks a transition for a pair nobody has edited: preset from how well they
/// match, the incoming song entering on its first downbeat, and the outgoing
/// song leaving on its last phrase boundary that leaves room for the window.
pub fn auto_transition(from: Side, to: Side, compat: &Compat) -> Transition {
    let (a, b) = (from.grid(), to.grid());
    let synced = matches!((a, b), (Some(a), Some(b)) if sync_rate(a.bpm, b.bpm).is_some());
    let (preset, bars) = auto_preset(a.is_some() && b.is_some(), synced, compat);
    let mut t = Transition {
        preset,
        volume: Volume::Smooth,
        eq: Eq::Off,
        effect: Effect::None,
        bars,
        out_ms: 0.0,
        in_ms: 0.0,
        sync: synced,
        auto: true,
    };
    t.set_preset(preset);

    t.in_ms = match b {
        // Small tolerance so a downbeat right at the onset still counts.
        Some(g) => g.beat_time((g.beat_at(to.start_ms() - 30.0) / 4.0).ceil().max(0.0) * 4.0),
        None => to.start_ms(),
    };
    let duration = t.timing(a.as_ref(), b.as_ref()).duration_ms;
    let latest = from.end_ms() - duration;
    t.out_ms = match a {
        Some(g) => {
            let beat = g.beat_at(latest);
            let phrase = f64::from(PHRASE_BARS * BEATS_PER_BAR);
            let on_phrase = g.beat_time((beat / phrase).floor() * phrase);
            // A phrase boundary far back would cut too much; fall back to a bar.
            if on_phrase >= from.end_ms() * 0.6 { on_phrase } else { g.beat_time((beat / 4.0).floor() * 4.0) }
        }
        None => latest,
    };
    t.clamp(from, to);
    t
}

fn auto_preset(gridded: bool, synced: bool, compat: &Compat) -> (Preset, u32) {
    if !gridded {
        return (Preset::Fade, 4);
    }
    if !synced {
        // Beats can't be lined up, so keep the overlap short.
        return (Preset::Fade, 2);
    }
    let lift = compat.energy.map_or(0.0, |e| e.delta());
    let key_ok = compat.key.is_none_or(|k| k.relation.score() >= 0.85);
    if lift >= 0.15 {
        (Preset::Rise, 8)
    } else if lift <= -0.2 {
        (Preset::Melt, 4)
    } else if key_ok {
        (Preset::Blend, 8)
    } else {
        // Filters keep clashing keys from fighting for long.
        (Preset::Wave, 4)
    }
}

fn smoothstep(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

/// 0 before `from`, 1 after `to`, smooth in between.
fn ramp(x: f32, from: f32, to: f32) -> f32 {
    smoothstep((x - from) / (to - from))
}

/// A quick switch centred on `at`.
fn step(x: f32, at: f32) -> f32 {
    ramp(x, at - 0.015, at + 0.015)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::{compat, tests::feat};

    fn transition(preset: Preset) -> Transition {
        let mut t = Transition {
            preset,
            volume: Volume::Smooth,
            eq: Eq::Off,
            effect: Effect::None,
            bars: 8,
            out_ms: 0.0,
            in_ms: 0.0,
            sync: true,
            auto: false,
        };
        t.set_preset(preset);
        t
    }

    #[test]
    fn every_curve_hands_over_completely() {
        for preset in Preset::ALL {
            let t = transition(preset);
            let (start, end) = (t.at(0.0), t.at(1.0));
            assert!((start.out.gain - 1.0).abs() < 1e-6, "{preset:?} starts with the outgoing song");
            assert!(end.out.gain.abs() < 1e-6, "{preset:?} ends without it");
            assert!((end.inc.gain - 1.0).abs() < 1e-6, "{preset:?} ends on the incoming song");
            assert_eq!(end.inc.eq, [1.0; 3], "{preset:?} leaves the incoming EQ flat");
            assert!(end.inc.filter.abs() < 1e-6, "{preset:?} leaves the incoming filter open");
        }
        for volume in Volume::ALL {
            let (a, b) = volume.gains(1.0);
            assert!(a.abs() < 1e-6 && (b - 1.0).abs() < 1e-6, "{volume:?}");
        }
    }

    #[test]
    fn smooth_crossfade_keeps_power() {
        let t = transition(Preset::Fade);
        for i in 0..=10 {
            let p = t.at(i as f32 / 10.0);
            assert!((p.out.gain.powi(2) + p.inc.gain.powi(2) - 1.0).abs() < 1e-5);
        }
    }

    #[test]
    fn center_bass_swaps_at_the_midpoint() {
        let t = transition(Preset::Fade);
        assert_eq!(t.at(0.4).out.eq[0], 1.0);
        assert_eq!(t.at(0.4).inc.eq[0], 0.0);
        assert_eq!(t.at(0.6).out.eq[0], 0.0);
        assert_eq!(t.at(0.6).inc.eq[0], 1.0);
    }

    #[test]
    fn editing_a_curve_makes_it_custom_until_it_matches_again() {
        let mut t = transition(Preset::Blend);
        t.effect = Effect::Echo;
        t.customize();
        assert_eq!(t.preset, Preset::Custom);
        t.set_preset(Preset::Rise);
        assert_eq!((t.volume, t.eq, t.effect), (Volume::Overlap, Eq::EndBass, Effect::FilterSwap));
        t.customize();
        assert_eq!(t.preset, Preset::Rise);
    }

    #[test]
    fn sync_folds_half_and_double_time_and_refuses_big_jumps() {
        assert_eq!(sync_rate(128.0, 128.0), Some(1.0));
        assert!((sync_rate(128.0, 124.0).unwrap() - 128.0 / 124.0).abs() < 1e-9);
        assert_eq!(sync_rate(87.0, 174.0), Some(1.0));
        assert_eq!(sync_rate(174.0, 87.0), Some(1.0));
        assert_eq!(sync_rate(128.0, 100.0), None);
    }

    #[test]
    fn timing_covers_the_same_beats_on_both_sides() {
        let mut t = transition(Preset::Blend);
        t.out_ms = 60_000.0;
        t.in_ms = 1_000.0;
        let a = Grid { bpm: 120.0, downbeat_ms: 0.0 };
        let b = Grid { bpm: 125.0, downbeat_ms: 0.0 };
        let tm = t.timing(Some(&a), Some(&b));
        assert_eq!(tm.duration_ms, 16_000.0); // 8 bars at 120
        assert!((tm.rate - 120.0 / 125.0).abs() < 1e-9);
        // 32 beats of B at 125 BPM.
        assert!((tm.in_end_ms - tm.in_start_ms - 32.0 * 480.0).abs() < 1e-6);
        t.sync = false;
        assert_eq!(t.timing(Some(&a), Some(&b)).rate, 1.0);
    }

    #[test]
    fn auto_enters_on_the_first_downbeat_and_leaves_on_a_phrase() {
        let mut fa = feat(120.0, 9, 0, 0.6);
        fa.grid = Some(Grid { bpm: 120.0, downbeat_ms: 100.0 });
        fa.end_ms = 200_000.0;
        let mut fb = feat(122.0, 4, 0, 0.6);
        fb.grid = Some(Grid { bpm: 122.0, downbeat_ms: 300.0 });
        fb.start_ms = 2_000.0;
        let c = compat(Some(&fa), Some(&fb));
        let from = Side { duration_ms: 205_000.0, analysis: Some(&fa) };
        let to = Side { duration_ms: 180_000.0, analysis: Some(&fb) };
        let t = auto_transition(from, to, &c);
        assert!(t.auto && t.sync);
        assert_eq!(t.preset, Preset::Blend);
        let ga = fa.grid.unwrap();
        let gb = fb.grid.unwrap();
        // Out: on a phrase (32 beats) and the window ends before the music does.
        let out_beat = ga.beat_at(t.out_ms);
        assert!((out_beat / 32.0 - (out_beat / 32.0).round()).abs() < 1e-9, "{out_beat}");
        let tm = t.timing(Some(&ga), Some(&gb));
        assert!(tm.out_end_ms <= 200_000.0 && tm.out_end_ms > 200_000.0 - 32.0 * 500.0 - 16_000.0);
        // In: first downbeat at or after the music starts.
        assert!(gb.is_downbeat(t.in_ms) && t.in_ms >= 2_000.0 - 30.0 && t.in_ms < 2_000.0 + gb.bar_ms());
    }

    #[test]
    fn auto_keeps_unsyncable_pairs_short() {
        let fa = feat(90.0, 9, 0, 0.6);
        let fb = feat(128.0, 9, 0, 0.6);
        let c = compat(Some(&fa), Some(&fb));
        let side = |f| Side { duration_ms: 200_000.0, analysis: Some(f) };
        let t = auto_transition(side(&fa), side(&fb), &c);
        assert!(!t.sync);
        assert_eq!(t.bars, 2);
        let none = auto_transition(Side { duration_ms: 200_000.0, analysis: None }, side(&fb), &c);
        assert_eq!((none.preset, none.bars, none.sync), (Preset::Fade, 4, false));
        assert_eq!(none.out_ms, 200_000.0 - 8_000.0);
    }

    #[test]
    fn auto_picks_by_energy_and_key() {
        let pick = |a: Analysis, b: Analysis| {
            let c = compat(Some(&a), Some(&b));
            let side = |f| Side { duration_ms: 200_000.0, analysis: Some(f) };
            auto_transition(side(&a), side(&b), &c).preset
        };
        assert_eq!(pick(feat(124.0, 9, 0, 0.4), feat(125.0, 9, 0, 0.7)), Preset::Rise);
        assert_eq!(pick(feat(124.0, 9, 0, 0.8), feat(125.0, 9, 0, 0.4)), Preset::Melt);
        assert_eq!(pick(feat(124.0, 9, 0, 0.6), feat(125.0, 3, 0, 0.6)), Preset::Wave);
    }

    #[test]
    fn clamp_keeps_the_window_inside_both_songs() {
        let mut t = transition(Preset::Blend);
        t.out_ms = 500_000.0;
        t.in_ms = 500_000.0;
        let g = Grid { bpm: 120.0, downbeat_ms: 0.0 };
        let mut f = feat(120.0, 9, 0, 0.5);
        f.grid = Some(g);
        let short = Side { duration_ms: 20_000.0, analysis: Some(&f) };
        t.clamp(Side { duration_ms: 200_000.0, analysis: Some(&f) }, short);
        // 8 bars (16 s) plus 5 s left over don't fit in 20 s, 4 bars do.
        assert_eq!(t.bars, 4);
        assert_eq!(t.out_ms, 200_000.0 - 8_000.0);
        assert_eq!(t.in_ms, 20_000.0 - MIN_REMAINING_MS - 8_000.0);
    }
}
