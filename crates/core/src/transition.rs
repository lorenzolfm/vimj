//! Transitions: how one song hands over to the next.
//!
//! Both songs play together for a whole number of bars of the outgoing song.
//! Across that window the same five ingredients as in Spotify's Mix shape the
//! handover: volume, EQ, a filter, an effect (reverb, echo, delay) and a loop
//! of the outgoing song. Each of the first four is a pair of curves, one per
//! song, picked from a named style and redrawn by hand if wanted. The
//! incoming song is time-stretched to the outgoing tempo while they overlap,
//! so their beats stay locked, then eases back to its own tempo.

use crate::compat::Compat;
use crate::curve::{self, Curve, Point, line, p};
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
    Dissolve,
    Surge,
    Afterglow,
}

/// One style per ingredient.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Styles {
    pub volume: Volume,
    pub eq: Eq,
    pub filter: Filter,
    pub effect: Effect,
    pub looping: Looping,
}

impl Preset {
    pub const ALL: [Preset; 9] = [
        Preset::Fade,
        Preset::Rise,
        Preset::Blend,
        Preset::Wave,
        Preset::Melt,
        Preset::Slam,
        Preset::Dissolve,
        Preset::Surge,
        Preset::Afterglow,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Fade => "Fade",
            Preset::Rise => "Rise",
            Preset::Blend => "Blend",
            Preset::Wave => "Wave",
            Preset::Melt => "Melt",
            Preset::Slam => "Slam",
            Preset::Dissolve => "Dissolve",
            Preset::Surge => "Surge",
            Preset::Afterglow => "Afterglow",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Preset::Fade => "Smooth crossfade, bass swaps at the midpoint",
            Preset::Rise => "Overlap, low-pass in and high-pass out, bass swaps at the end",
            Preset::Blend => "Overlap with a smooth three-band EQ handover",
            Preset::Wave => "Overlap, low-pass filter in, bass swaps at the midpoint",
            Preset::Melt => "Crossfade, high-pass filter in, bass swaps at the midpoint",
            Preset::Slam => "Hard volume swap at the midpoint",
            Preset::Dissolve => "Crossfade, low-pass out into a reverb that rings on",
            Preset::Surge => "A one-bar loop and a noise sweep build up, then cut",
            Preset::Afterglow => "Smooth crossfade, high-pass out, an echo that rings on",
        }
    }

    pub fn styles(self) -> Styles {
        let styles = |volume, eq, filter, effect, looping| Styles { volume, eq, filter, effect, looping };
        match self {
            Preset::Fade => styles(Volume::Smooth, Eq::CenterBass, Filter::None, Effect::None, Looping::None),
            Preset::Rise => styles(Volume::Overlap, Eq::EndBass, Filter::LowPassInHighPassOut, Effect::None, Looping::None),
            Preset::Blend => styles(Volume::Overlap, Eq::ThreeBand, Filter::None, Effect::None, Looping::None),
            Preset::Wave => styles(Volume::Overlap, Eq::CenterBass, Filter::LowPassIn, Effect::None, Looping::None),
            Preset::Melt => styles(Volume::Crossfade, Eq::CenterBass, Filter::HighPassIn, Effect::None, Looping::None),
            Preset::Slam => styles(Volume::CenterCut, Eq::Off, Filter::None, Effect::None, Looping::None),
            Preset::Dissolve => styles(Volume::Crossfade, Eq::CenterBass, Filter::LowPassOut, Effect::ReverbOutEnd, Looping::None),
            Preset::Surge => styles(Volume::FadeInCutOut, Eq::EndBass, Filter::NoiseOutEnd, Effect::None, Looping::FourBeat),
            Preset::Afterglow => styles(Volume::Smooth, Eq::CenterBass, Filter::HighPassOut, Effect::EchoThreeQuarterOutEnd, Looping::None),
        }
    }
}

/// The four ingredients drawn as curves. Looping has none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lane {
    Volume,
    Eq,
    Filter,
    Effect,
}

impl Lane {
    pub const ALL: [Lane; 4] = [Lane::Volume, Lane::Eq, Lane::Filter, Lane::Effect];

    pub fn label(self) -> &'static str {
        match self {
            Lane::Volume => "Volume",
            Lane::Eq => "EQ",
            Lane::Filter => "Filter",
            Lane::Effect => "Effects",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Track {
    /// The outgoing song, A.
    Out,
    /// The incoming song, B.
    In,
}

impl Track {
    fn index(self) -> usize {
        match self {
            Track::Out => 0,
            Track::In => 1,
        }
    }
}

/// A curve for each song: outgoing, incoming.
type Pair = [&'static [Point]; 2];

const FULL: &[Point] = &[p(0.0, 1.0), p(1.0, 1.0)];
const DOWN: &[Point] = &[p(0.0, 1.0), p(1.0, 0.0)];
const UP: &[Point] = &[p(0.0, 0.0), p(1.0, 1.0)];
/// Quick switches, 3% of the window wide, centred where they happen.
const OFF_AT_START: &[Point] = &[p(0.0, 1.0), p(0.035, 1.0), p(0.065, 0.0), p(1.0, 0.0)];
const OFF_AT_10: &[Point] = &[p(0.0, 1.0), p(0.085, 1.0), p(0.115, 0.0), p(1.0, 0.0)];
const OFF_AT_40: &[Point] = &[p(0.0, 1.0), p(0.385, 1.0), p(0.415, 0.0), p(1.0, 0.0)];
const OFF_AT_HALF: &[Point] = &[p(0.0, 1.0), p(0.485, 1.0), p(0.515, 0.0), p(1.0, 0.0)];
const OFF_AT_90: &[Point] = &[p(0.0, 1.0), p(0.885, 1.0), p(0.915, 0.0), p(1.0, 0.0)];
const ON_AT_10: &[Point] = &[p(0.0, 0.0), p(0.085, 0.0), p(0.115, 1.0), p(1.0, 1.0)];
const ON_AT_HALF: &[Point] = &[p(0.0, 0.0), p(0.485, 0.0), p(0.515, 1.0), p(1.0, 1.0)];
const ON_AT_60: &[Point] = &[p(0.0, 0.0), p(0.585, 0.0), p(0.615, 1.0), p(1.0, 1.0)];
const ON_AT_90: &[Point] = &[p(0.0, 0.0), p(0.885, 0.0), p(0.915, 1.0), p(1.0, 1.0)];
const ON_AT_END: &[Point] = &[p(0.0, 0.0), p(0.935, 0.0), p(0.965, 1.0), p(1.0, 1.0)];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Volume {
    Smooth,
    Overlap,
    FadeInFadeOut,
    CutInFadeOut,
    FadeInCutOut,
    CenterCut,
    Crossfade,
    FadeInFastOut,
}

impl Volume {
    pub const ALL: [Volume; 8] = [
        Volume::Smooth,
        Volume::Overlap,
        Volume::FadeInFadeOut,
        Volume::CutInFadeOut,
        Volume::FadeInCutOut,
        Volume::CenterCut,
        Volume::Crossfade,
        Volume::FadeInFastOut,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Volume::Smooth => "Smooth crossfade",
            Volume::Overlap => "Overlap",
            Volume::FadeInFadeOut => "Fade in fade out",
            Volume::CutInFadeOut => "Cut in fade out",
            Volume::FadeInCutOut => "Fade in cut out",
            Volume::CenterCut => "Center cut",
            Volume::Crossfade => "Crossfade",
            Volume::FadeInFastOut => "Fade in fast out",
        }
    }

    /// Gain curves of the outgoing and incoming song.
    fn curves(self) -> Pair {
        // Equal power, cos and sin, to within 1%.
        const SMOOTH_OUT: &[Point] = &[line(0.0, 1.0, 0.1464), line(0.5, std::f32::consts::FRAC_1_SQRT_2, 0.058), p(1.0, 0.0)];
        const SMOOTH_IN: &[Point] = &[line(0.0, 0.0, 0.058), line(0.5, std::f32::consts::FRAC_1_SQRT_2, 0.1464), p(1.0, 1.0)];
        const LATE_OUT: &[Point] = &[p(0.0, 1.0), p(0.85, 1.0), p(1.0, 0.0)];
        const EARLY_IN: &[Point] = &[p(0.0, 0.0), p(0.15, 1.0), p(1.0, 1.0)];
        const SECOND_HALF_OUT: &[Point] = &[p(0.0, 1.0), p(0.5, 1.0), p(1.0, 0.0)];
        const FIRST_HALF_IN: &[Point] = &[p(0.0, 0.0), p(0.5, 1.0), p(1.0, 1.0)];
        const CUT_OUT: &[Point] = &[p(0.0, 1.0), p(0.97, 1.0), p(1.0, 0.0)];
        const FAST_OUT: &[Point] = &[p(0.0, 1.0), p(0.8, 1.0), p(1.0, 0.0)];
        const LINEAR_OUT: &[Point] = &[line(0.0, 1.0, 0.0), p(1.0, 0.0)];
        const LINEAR_IN: &[Point] = &[line(0.0, 0.0, 0.0), p(1.0, 1.0)];
        match self {
            Volume::Smooth => [SMOOTH_OUT, SMOOTH_IN],
            Volume::Overlap => [LATE_OUT, EARLY_IN],
            Volume::FadeInFadeOut => [SECOND_HALF_OUT, FIRST_HALF_IN],
            Volume::CutInFadeOut => [DOWN, FULL],
            Volume::FadeInCutOut => [CUT_OUT, UP],
            Volume::CenterCut => [OFF_AT_HALF, ON_AT_HALF],
            Volume::Crossfade => [LINEAR_OUT, LINEAR_IN],
            Volume::FadeInFastOut => [FAST_OUT, UP],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Eq {
    CenterBass,
    EndBass,
    StartBass,
    ThreeBand,
    QuickBassCut,
    LongBassCut,
    StartFadeOut,
    BassFadeOut,
    Off,
}

impl Eq {
    pub const ALL: [Eq; 9] = [
        Eq::CenterBass,
        Eq::EndBass,
        Eq::StartBass,
        Eq::ThreeBand,
        Eq::QuickBassCut,
        Eq::LongBassCut,
        Eq::StartFadeOut,
        Eq::BassFadeOut,
        Eq::Off,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Eq::CenterBass => "Center bass swap",
            Eq::EndBass => "End bass swap",
            Eq::StartBass => "Start bass swap",
            Eq::ThreeBand => "3-band fade",
            Eq::QuickBassCut => "Quick bass cut",
            Eq::LongBassCut => "Long bass cut",
            Eq::StartFadeOut => "Start fade out",
            Eq::BassFadeOut => "Bass fade out",
            Eq::Off => "None",
        }
    }

    /// Low, mid and high band curves (1 flat, 0 killed) of the outgoing and
    /// incoming song. Only the low band can be redrawn.
    fn bands(self) -> [[&'static [Point]; 3]; 2] {
        // Highs hand over first, then mids, then lows.
        const LOW_OUT: &[Point] = &[p(0.0, 1.0), p(0.6, 1.0), p(1.0, 0.0)];
        const MID_OUT: &[Point] = &[p(0.0, 1.0), p(0.3, 1.0), p(0.7, 0.0), p(1.0, 0.0)];
        const HIGH_OUT: &[Point] = &[p(0.0, 1.0), p(0.4, 0.0), p(1.0, 0.0)];
        const LOW_IN: &[Point] = &[p(0.0, 0.0), p(0.6, 0.0), p(1.0, 1.0)];
        const MID_IN: &[Point] = &[p(0.0, 0.0), p(0.3, 0.0), p(0.7, 1.0), p(1.0, 1.0)];
        const HIGH_IN: &[Point] = &[p(0.0, 0.0), p(0.4, 1.0), p(1.0, 1.0)];
        const FIRST_HALF_OUT: &[Point] = &[p(0.0, 1.0), p(0.5, 0.0), p(1.0, 0.0)];
        let bass = |out, inc| [[out, FULL, FULL], [inc, FULL, FULL]];
        match self {
            Eq::CenterBass => bass(OFF_AT_HALF, ON_AT_HALF),
            Eq::EndBass => bass(OFF_AT_90, ON_AT_90),
            Eq::StartBass => bass(OFF_AT_10, ON_AT_10),
            Eq::ThreeBand => [[LOW_OUT, MID_OUT, HIGH_OUT], [LOW_IN, MID_IN, HIGH_IN]],
            // Neither song has bass for a moment in the middle…
            Eq::QuickBassCut => bass(OFF_AT_40, ON_AT_60),
            // …or for nearly the whole window.
            Eq::LongBassCut => bass(OFF_AT_START, ON_AT_END),
            Eq::StartFadeOut => bass(FIRST_HALF_OUT, ON_AT_HALF),
            Eq::BassFadeOut => bass(DOWN, ON_AT_END),
            Eq::Off => bass(FULL, FULL),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Filter {
    LowPassOut,
    LowPassIn,
    LowPassInLowPassOut,
    LowPassInHighPassOut,
    HighPassOut,
    HighPassIn,
    HighPassInHighPassOut,
    HighPassInLowPassOut,
    HighPassHalfOut,
    NoiseOutEnd,
    None,
}

/// What a song's filter curve drives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FilterKind {
    /// The curve closes a low-pass: 0 open, 1 shut.
    LowPass,
    /// The curve closes a high-pass: 0 open, 1 shut.
    HighPass,
    /// Filtered noise over the song: the curve is its level and brightness.
    Noise,
}

/// How far the filter closes in the sweeps.
const SWEEP: f32 = 0.8;

impl Filter {
    pub const ALL: [Filter; 11] = [
        Filter::LowPassOut,
        Filter::LowPassIn,
        Filter::LowPassInLowPassOut,
        Filter::LowPassInHighPassOut,
        Filter::HighPassOut,
        Filter::HighPassIn,
        Filter::HighPassInHighPassOut,
        Filter::HighPassInLowPassOut,
        Filter::HighPassHalfOut,
        Filter::NoiseOutEnd,
        Filter::None,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Filter::LowPassOut => "Low pass filter out",
            Filter::LowPassIn => "Low pass filter in",
            Filter::LowPassInLowPassOut => "Low pass filter in low pass filter out",
            Filter::LowPassInHighPassOut => "Low pass filter in high pass filter out",
            Filter::HighPassOut => "High pass filter out",
            Filter::HighPassIn => "High pass filter in",
            Filter::HighPassInHighPassOut => "High pass filter in high pass filter out",
            Filter::HighPassInLowPassOut => "High pass filter in low pass filter out",
            Filter::HighPassHalfOut => "High-pass filter half out",
            Filter::NoiseOutEnd => "Noise out end",
            Filter::None => "None",
        }
    }

    /// The filter on each song, and its curve: outgoing songs close, incoming ones open.
    fn shape(self) -> [Option<(FilterKind, &'static [Point])>; 2] {
        use FilterKind::{HighPass, LowPass, Noise};
        const CLOSE: &[Point] = &[p(0.0, 0.0), p(1.0, SWEEP)];
        const OPEN: &[Point] = &[p(0.0, SWEEP), p(1.0, 0.0)];
        const HALF: &[Point] = &[p(0.0, 0.0), p(1.0, SWEEP / 2.0)];
        const RISE: &[Point] = &[p(0.0, 0.0), p(0.5, 0.0), p(1.0, 1.0)];
        match self {
            Filter::LowPassOut => [Some((LowPass, CLOSE)), None],
            Filter::LowPassIn => [None, Some((LowPass, OPEN))],
            Filter::LowPassInLowPassOut => [Some((LowPass, CLOSE)), Some((LowPass, OPEN))],
            Filter::LowPassInHighPassOut => [Some((HighPass, CLOSE)), Some((LowPass, OPEN))],
            Filter::HighPassOut => [Some((HighPass, CLOSE)), None],
            Filter::HighPassIn => [None, Some((HighPass, OPEN))],
            Filter::HighPassInHighPassOut => [Some((HighPass, CLOSE)), Some((HighPass, OPEN))],
            Filter::HighPassInLowPassOut => [Some((LowPass, CLOSE)), Some((HighPass, OPEN))],
            Filter::HighPassHalfOut => [Some((HighPass, HALF)), None],
            Filter::NoiseOutEnd => [Some((Noise, RISE)), None],
            Filter::None => [None, None],
        }
    }

    pub fn kind(self, track: Track) -> Option<FilterKind> {
        self.shape()[track.index()].map(|(kind, _)| kind)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    ReverbOutCenter,
    ReverbCutEnd,
    ReverbOutEnd,
    DelayHalfCutEnd,
    DelayThreeQuarterCutEnd,
    EchoHalfCutEnd,
    EchoHalfOutEnd,
    EchoThreeQuarterCutEnd,
    EchoThreeQuarterOutEnd,
    EchoOneCutEnd,
    EchoOneOutEnd,
    None,
}

/// The sound an effect makes. Echo and delay repeat at a fraction of a beat;
/// echo keeps repeating, quieter each time, where delay repeats once.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EffectKind {
    Reverb,
    Echo { beats: f32 },
    Delay { beats: f32 },
}

impl Effect {
    pub const ALL: [Effect; 12] = [
        Effect::ReverbOutCenter,
        Effect::ReverbCutEnd,
        Effect::ReverbOutEnd,
        Effect::DelayHalfCutEnd,
        Effect::DelayThreeQuarterCutEnd,
        Effect::EchoHalfCutEnd,
        Effect::EchoHalfOutEnd,
        Effect::EchoThreeQuarterCutEnd,
        Effect::EchoThreeQuarterOutEnd,
        Effect::EchoOneCutEnd,
        Effect::EchoOneOutEnd,
        Effect::None,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Effect::ReverbOutCenter => "Reverb out center",
            Effect::ReverbCutEnd => "Reverb cut end",
            Effect::ReverbOutEnd => "Reverb out end",
            Effect::DelayHalfCutEnd => "Delay ½ cut end",
            Effect::DelayThreeQuarterCutEnd => "Delay ¾ cut end",
            Effect::EchoHalfCutEnd => "Echo ½ cut end",
            Effect::EchoHalfOutEnd => "Echo ½ out end",
            Effect::EchoThreeQuarterCutEnd => "Echo ¾ cut end",
            Effect::EchoThreeQuarterOutEnd => "Echo ¾ out end",
            Effect::EchoOneCutEnd => "Echo 1 cut end",
            Effect::EchoOneOutEnd => "Echo 1 out end",
            Effect::None => "None",
        }
    }

    /// What the effect is, and its send curve on the outgoing song. Effects
    /// only ever go on the outgoing song.
    pub fn shape(self) -> Option<(EffectKind, &'static [Point])> {
        // The send stops at the midpoint and the reverb rings out from there.
        const CENTER: &[Point] = &[p(0.0, 0.0), p(0.485, 1.0), p(0.515, 0.0), p(1.0, 0.0)];
        const LATE: &[Point] = &[p(0.0, 0.0), p(0.5, 0.0), p(0.9, 1.0), p(1.0, 1.0)];
        let echo = |beats| Some((EffectKind::Echo { beats }, LATE));
        let delay = |beats| Some((EffectKind::Delay { beats }, LATE));
        match self {
            Effect::ReverbOutCenter => Some((EffectKind::Reverb, CENTER)),
            Effect::ReverbCutEnd | Effect::ReverbOutEnd => Some((EffectKind::Reverb, UP)),
            Effect::DelayHalfCutEnd => delay(0.5),
            Effect::DelayThreeQuarterCutEnd => delay(0.75),
            Effect::EchoHalfCutEnd | Effect::EchoHalfOutEnd => echo(0.5),
            Effect::EchoThreeQuarterCutEnd | Effect::EchoThreeQuarterOutEnd => echo(0.75),
            Effect::EchoOneCutEnd | Effect::EchoOneOutEnd => echo(1.0),
            Effect::None => None,
        }
    }

    pub fn kind(self) -> Option<EffectKind> {
        self.shape().map(|(kind, _)| kind)
    }

    /// Whether the effect rings on after the window ends ("out"), rather
    /// than being cut there ("cut").
    pub fn rings_out(self) -> bool {
        matches!(self, Effect::ReverbOutCenter | Effect::ReverbOutEnd | Effect::EchoHalfOutEnd | Effect::EchoThreeQuarterOutEnd | Effect::EchoOneOutEnd)
    }
}

/// Repeating a few beats of the outgoing song from the start of the window
/// until its end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Looping {
    OneBeat,
    TwoBeat,
    FourBeat,
    EightBeat,
    SixteenBeat,
    /// The loop halves in length as the window goes on.
    Halving,
    #[default]
    None,
}

/// Part of the window looping `beats` beats from the window's start.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopBlock {
    /// Fractions of the window.
    pub start: f64,
    pub end: f64,
    pub beats: f64,
}

impl Looping {
    pub const ALL: [Looping; 7] =
        [Looping::OneBeat, Looping::TwoBeat, Looping::FourBeat, Looping::EightBeat, Looping::SixteenBeat, Looping::Halving, Looping::None];

    pub fn label(self) -> &'static str {
        match self {
            Looping::OneBeat => "1 beat loop",
            Looping::TwoBeat => "2 beat loop",
            Looping::FourBeat => "4 beat loop",
            Looping::EightBeat => "8 beat loop",
            Looping::SixteenBeat => "16 beat loop",
            Looping::Halving => "Loop halving",
            Looping::None => "None",
        }
    }

    /// How the loop runs through a window of `window_beats`.
    pub fn blocks(self, window_beats: f64) -> Vec<LoopBlock> {
        let whole = |beats| vec![LoopBlock { start: 0.0, end: 1.0, beats }];
        match self {
            Looping::OneBeat => whole(1.0),
            Looping::TwoBeat => whole(2.0),
            Looping::FourBeat => whole(4.0),
            Looping::EightBeat => whole(8.0),
            Looping::SixteenBeat => whole(16.0),
            // An eighth of the window, four times; then half that, four
            // times; and so on, ending in a roll.
            Looping::Halving => [(0.0, 0.5), (0.5, 0.75), (0.75, 0.875), (0.875, 1.0)]
                .into_iter()
                .zip([8.0, 16.0, 32.0, 64.0])
                .map(|((start, end), part)| LoopBlock { start, end, beats: window_beats / part })
                .collect(),
            Looping::None => Vec::new(),
        }
    }
}

/// Where a loop sends the outgoing song. Times are on the timeline the song
/// would have played without the loop; from `start`, each stage replays the
/// song from `start` again every `period`.
#[derive(Clone, Debug, PartialEq)]
pub struct LoopMap {
    pub start: f64,
    pub stages: Vec<Stage>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stage {
    pub from: f64,
    pub to: f64,
    pub period: f64,
}

impl LoopMap {
    /// The same loop in other units, `factor` of them to each of these.
    pub fn scaled(&self, factor: f64) -> LoopMap {
        LoopMap {
            start: self.start * factor,
            stages: self.stages.iter().map(|s| Stage { from: s.from * factor, to: s.to * factor, period: s.period * factor }).collect(),
        }
    }

    /// The start of the pass through the loop that `t` is in.
    fn pass(&self, t: f64) -> Option<(f64, f64)> {
        if t < self.start {
            return None;
        }
        let stage = self.stages.iter().find(|s| t < s.to).or(self.stages.last())?;
        let n = ((t - stage.from) / stage.period).floor().max(0.0);
        Some((stage.from + n * stage.period, stage.period))
    }

    /// What `t` plays: the song at `t` plus this.
    pub fn offset(&self, t: f64) -> f64 {
        self.pass(t).map_or(0.0, |(pass, _)| self.start - pass)
    }

    /// The offset at `t` and, after a jump back to the start of the loop, the
    /// offset before it and how long ago the jump was.
    pub fn jump(&self, t: f64) -> (f64, Option<(f64, f64)>) {
        let Some((pass, period)) = self.pass(t) else { return (0.0, None) };
        let now = self.start - pass;
        let before = self.offset(pass - period * 1e-6);
        (now, (before != now).then_some((before, t - pass)))
    }

    /// Whether `t` plays a repeat rather than the song's first time through.
    pub fn repeating(&self, t: f64) -> bool {
        self.offset(t) < 0.0
    }
}

/// Curves drawn by hand, each in place of its style's for one song. Effects
/// only ever go on the outgoing song, so that lane has its one curve.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Curves {
    #[serde(default, skip_serializing_if = "unset")]
    volume: [Option<Curve>; 2],
    #[serde(default, skip_serializing_if = "unset")]
    eq: [Option<Curve>; 2],
    #[serde(default, skip_serializing_if = "unset")]
    filter: [Option<Curve>; 2],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effect: Option<Curve>,
}

fn unset(pair: &[Option<Curve>; 2]) -> bool {
    pair.iter().all(Option::is_none)
}

impl Curves {
    pub fn is_empty(&self) -> bool {
        unset(&self.volume) && unset(&self.eq) && unset(&self.filter) && self.effect.is_none()
    }

    /// Where a lane's curve for one song goes: nowhere for effects on the incoming song.
    fn slot_mut(&mut self, lane: Lane, track: Track) -> Option<&mut Option<Curve>> {
        let i = track.index();
        match lane {
            Lane::Volume => Some(&mut self.volume[i]),
            Lane::Eq => Some(&mut self.eq[i]),
            Lane::Filter => Some(&mut self.filter[i]),
            Lane::Effect => (track == Track::Out).then_some(&mut self.effect),
        }
    }

    fn clear(&mut self, lane: Lane) {
        match lane {
            Lane::Volume => self.volume = [None, None],
            Lane::Eq => self.eq = [None, None],
            Lane::Filter => self.filter = [None, None],
            Lane::Effect => self.effect = None,
        }
    }

    pub fn get(&self, lane: Lane, track: Track) -> Option<&[Point]> {
        let i = track.index();
        match lane {
            Lane::Volume => self.volume[i].as_deref(),
            Lane::Eq => self.eq[i].as_deref(),
            Lane::Filter => self.filter[i].as_deref(),
            Lane::Effect => self.effect.as_deref().filter(|_| track == Track::Out),
        }
    }
}

/// Settings for one deck at one moment of a transition.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeckFx {
    pub gain: f32,
    /// Low, mid, high: 1 is flat, 0 kills the band.
    pub eq: [f32; 3],
    /// Filter knob: below 0 low-pass, above 0 high-pass, 0 open; ±1 fully closed.
    pub filter: f32,
    /// Noise sweep level and brightness, 0..=1.
    pub noise: f32,
    /// Effect send, 0..=1.
    pub send: f32,
}

impl DeckFx {
    pub const FLAT: DeckFx = DeckFx { gain: 1.0, eq: [1.0; 3], filter: 0.0, noise: 0.0, send: 0.0 };
}

/// Both decks at one moment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Moment {
    pub out: DeckFx,
    pub inc: DeckFx,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Stored", into = "Stored")]
pub struct Transition {
    pub volume: Volume,
    pub eq: Eq,
    pub filter: Filter,
    pub effect: Effect,
    pub looping: Looping,
    pub curves: Curves,
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
    pub fn styles(&self) -> Styles {
        Styles { volume: self.volume, eq: self.eq, filter: self.filter, effect: self.effect, looping: self.looping }
    }

    /// The preset this is, or `None` for one of your own: styles no preset
    /// combines, or curves drawn by hand.
    pub fn preset(&self) -> Option<Preset> {
        let styles = self.styles();
        Preset::ALL.into_iter().find(|p| p.styles() == styles).filter(|_| self.curves.is_empty())
    }

    /// The preset's name, or "Custom".
    pub fn preset_label(&self) -> &'static str {
        self.preset().map_or("Custom", Preset::label)
    }

    /// Takes on a preset's styles, dropping curves drawn by hand.
    pub fn set_preset(&mut self, preset: Preset) {
        let s = preset.styles();
        (self.volume, self.eq, self.filter, self.effect, self.looping) = (s.volume, s.eq, s.filter, s.effect, s.looping);
        self.curves = Curves::default();
    }

    // A new style replaces the curves drawn over the old one.

    pub fn set_volume(&mut self, volume: Volume) {
        self.volume = volume;
        self.curves.clear(Lane::Volume);
    }

    pub fn set_eq(&mut self, eq: Eq) {
        self.eq = eq;
        self.curves.clear(Lane::Eq);
    }

    pub fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
        self.curves.clear(Lane::Filter);
    }

    pub fn set_effect(&mut self, effect: Effect) {
        self.effect = effect;
        self.curves.clear(Lane::Effect);
    }

    /// The curve a lane follows for one song: drawn by hand or its style's.
    /// `None` where the style leaves that song alone.
    pub fn curve(&self, lane: Lane, track: Track) -> Option<&[Point]> {
        let style = self.style_curve(lane, track)?;
        Some(self.curves.get(lane, track).unwrap_or(style))
    }

    fn style_curve(&self, lane: Lane, track: Track) -> Option<&'static [Point]> {
        let i = track.index();
        match lane {
            Lane::Volume => Some(self.volume.curves()[i]),
            Lane::Eq => Some(self.eq.bands()[i][0]),
            Lane::Filter => self.filter.shape()[i].map(|(_, c)| c),
            Lane::Effect => self.effect.shape().filter(|_| track == Track::Out).map(|(_, c)| c),
        }
    }

    /// Redraws a lane's curve for one song with `f`, starting from the
    /// style's, and returns what `f` does; `None` where the style leaves that
    /// song alone. A curve drawn back to the style's shape is the style again.
    pub fn redraw<R>(&mut self, lane: Lane, track: Track, f: impl FnOnce(&mut Curve) -> R) -> Option<R> {
        let style = self.style_curve(lane, track)?;
        let slot = self.curves.slot_mut(lane, track)?;
        let curve = slot.get_or_insert_with(|| Curve::try_from(style.to_vec()).expect("styles draw whole curves"));
        let done = f(curve);
        if **curve == *style {
            *slot = None;
        }
        Some(done)
    }

    /// Goes back to the style's curve for a lane and song.
    pub fn reset_curve(&mut self, lane: Lane, track: Track) {
        if let Some(slot) = self.curves.slot_mut(lane, track) {
            *slot = None;
        }
    }

    /// Whether a lane's curve for a song was drawn by hand.
    pub fn is_custom(&self, lane: Lane, track: Track) -> bool {
        self.style_curve(lane, track).is_some() && self.curves.get(lane, track).is_some()
    }

    /// Deck settings at progress `x` (0 at the start of the window, 1 at its end).
    pub fn at(&self, x: f32) -> Moment {
        let bands = self.eq.bands();
        let deck = |track: Track| {
            let value = |lane| self.curve(lane, track).map(|c| curve::value(c, x));
            let band = |b: usize| curve::value(bands[track.index()][b], x);
            let mut fx = DeckFx {
                gain: value(Lane::Volume).unwrap_or(1.0),
                eq: [value(Lane::Eq).unwrap_or(1.0), band(1), band(2)],
                send: value(Lane::Effect).unwrap_or(0.0),
                ..DeckFx::FLAT
            };
            if let (Some(kind), Some(amount)) = (self.filter.kind(track), value(Lane::Filter)) {
                match kind {
                    FilterKind::LowPass => fx.filter = -amount,
                    FilterKind::HighPass => fx.filter = amount,
                    FilterKind::Noise => fx.noise = amount,
                }
            }
            fx
        };
        Moment { out: deck(Track::Out), inc: deck(Track::In) }
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

    /// Where the loop sends the outgoing song, in its milliseconds; `from` is its grid.
    pub fn loop_map(&self, from: Option<&Grid>) -> Option<LoopMap> {
        let beat = from.map_or(60_000.0 / FALLBACK_BPM, Grid::beat_ms);
        let window_beats = f64::from(self.bars * BEATS_PER_BAR);
        let blocks = self.looping.blocks(window_beats);
        if blocks.is_empty() {
            return None;
        }
        let span = window_beats * beat;
        let stages = blocks
            .iter()
            .map(|b| Stage { from: self.out_ms + b.start * span, to: self.out_ms + b.end * span, period: b.beats * beat })
            .collect();
        Some(LoopMap { start: self.out_ms, stages })
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

/// A transition as stored and sent. Stored ones may be from before filter,
/// effects and looping were separate: `effect` then held a filter sweep, an
/// echo or a brake.
#[derive(Serialize, Deserialize)]
struct Stored {
    /// The preset the styles make, for frontends. Worked out again on
    /// reading, so a transition keeps its sound when a preset's recipe changes.
    #[serde(skip_deserializing)]
    preset: Option<Preset>,
    volume: Volume,
    eq: Eq,
    #[serde(default)]
    filter: Option<Filter>,
    effect: StoredEffect,
    #[serde(default)]
    looping: Looping,
    #[serde(default, skip_serializing_if = "Curves::is_empty")]
    curves: Curves,
    bars: u32,
    out_ms: f64,
    in_ms: f64,
    sync: bool,
    auto: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum StoredEffect {
    Current(Effect),
    Old(OldEffect),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OldEffect {
    HighPassOut,
    LowPassIn,
    FilterSwap,
    LowPassBoth,
    HighPassBoth,
    Echo,
    FilterEcho,
    Brake,
}

/// Checks what the types can't: the length, and that curves drawn by hand
/// are where a style draws one.
impl TryFrom<Stored> for Transition {
    type Error = String;

    fn try_from(s: Stored) -> Result<Transition, String> {
        if !BAR_CHOICES.contains(&s.bars) {
            return Err(format!("a transition lasts 1, 2, 4, 8 or 16 bars, not {}", s.bars));
        }
        let (filter, effect) = match s.effect {
            StoredEffect::Current(effect) => (s.filter.unwrap_or(Filter::None), effect),
            StoredEffect::Old(old) => match old {
                OldEffect::HighPassOut => (Filter::HighPassOut, Effect::None),
                OldEffect::LowPassIn => (Filter::LowPassIn, Effect::None),
                OldEffect::FilterSwap => (Filter::LowPassInHighPassOut, Effect::None),
                OldEffect::LowPassBoth => (Filter::LowPassInLowPassOut, Effect::None),
                OldEffect::HighPassBoth => (Filter::HighPassInHighPassOut, Effect::None),
                OldEffect::Echo => (Filter::None, Effect::EchoThreeQuarterOutEnd),
                OldEffect::FilterEcho => (Filter::HighPassOut, Effect::EchoThreeQuarterOutEnd),
                OldEffect::Brake => (Filter::None, Effect::None),
            },
        };
        let mut t = Transition {
            volume: s.volume,
            eq: s.eq,
            filter,
            effect,
            looping: s.looping,
            curves: s.curves,
            bars: s.bars,
            out_ms: s.out_ms,
            in_ms: s.in_ms,
            sync: s.sync,
            auto: s.auto,
        };
        for lane in Lane::ALL {
            for track in [Track::Out, Track::In] {
                let style = t.style_curve(lane, track);
                let Some(slot) = t.curves.slot_mut(lane, track) else { continue };
                match (slot.as_deref(), style) {
                    (Some(_), None) => {
                        let song = if track == Track::Out { "outgoing" } else { "incoming" };
                        return Err(format!("{} drawn by hand on the {song} song, whose style draws none", lane.label()));
                    }
                    // A curve drawn to the style's shape is the style.
                    (Some(drawn), Some(style)) if drawn == style => *slot = None,
                    _ => {}
                }
            }
        }
        Ok(t)
    }
}

impl From<Transition> for Stored {
    fn from(t: Transition) -> Stored {
        Stored {
            preset: t.preset(),
            volume: t.volume,
            eq: t.eq,
            filter: Some(t.filter),
            effect: StoredEffect::Current(t.effect),
            looping: t.looping,
            curves: t.curves,
            bars: t.bars,
            out_ms: t.out_ms,
            in_ms: t.in_ms,
            sync: t.sync,
            auto: t.auto,
        }
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
        volume: Volume::Smooth,
        eq: Eq::Off,
        filter: Filter::None,
        effect: Effect::None,
        looping: Looping::None,
        curves: Curves::default(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::{compat, tests::feat};

    fn transition(preset: Preset) -> Transition {
        let mut t = Transition {
            volume: Volume::Smooth,
            eq: Eq::Off,
            filter: Filter::None,
            effect: Effect::None,
            looping: Looping::None,
            curves: Curves::default(),
            bars: 8,
            out_ms: 0.0,
            in_ms: 0.0,
            sync: true,
            auto: false,
        };
        t.set_preset(preset);
        t
    }

    /// Every combination of styles, one ingredient changed at a time.
    fn every_style() -> Vec<Transition> {
        let base = transition(Preset::Fade);
        let mut all = Vec::new();
        for v in Volume::ALL {
            all.push(Transition { volume: v, ..base.clone() });
        }
        for e in Eq::ALL {
            all.push(Transition { eq: e, ..base.clone() });
        }
        for f in Filter::ALL {
            all.push(Transition { filter: f, ..base.clone() });
        }
        for e in Effect::ALL {
            all.push(Transition { effect: e, ..base.clone() });
        }
        all.extend(Preset::ALL.map(transition));
        all
    }

    #[test]
    fn every_style_hands_over_completely() {
        for t in every_style() {
            let (start, end) = (t.at(0.0), t.at(1.0));
            let what = t.styles();
            assert!((start.out.gain - 1.0).abs() < 1e-6, "{what:?} starts with the outgoing song");
            assert!(end.out.gain.abs() < 1e-6, "{what:?} ends without it");
            assert!((end.inc.gain - 1.0).abs() < 1e-6, "{what:?} ends on the incoming song");
            assert_eq!(start.out.eq[1..], [1.0; 2], "{what:?} starts with the outgoing mids and highs");
            assert_eq!(end.inc.eq, [1.0; 3], "{what:?} leaves the incoming EQ flat");
            assert!(end.inc.filter.abs() < 1e-6 && end.inc.noise == 0.0, "{what:?} leaves the incoming filter open");
            assert_eq!((start.out.filter, start.inc.send, end.inc.send), (0.0, 0.0, 0.0), "{what:?}");
            // Redrawing starts from these.
            for (lane, track) in Lane::ALL.into_iter().flat_map(|lane| [(lane, Track::Out), (lane, Track::In)]) {
                if let Some(c) = t.curve(lane, track) {
                    assert!(Curve::try_from(c.to_vec()).is_ok(), "{what:?}: {lane:?} on {track:?}");
                }
            }
            for i in 0..=20 {
                let m = t.at(i as f32 / 20.0);
                for fx in [m.out, m.inc] {
                    let in_range = [fx.gain, fx.noise, fx.send, fx.filter.abs()].into_iter().chain(fx.eq).all(|v| (0.0..=1.0).contains(&v));
                    assert!(in_range, "{what:?} at {i}: {fx:?}");
                }
            }
        }
    }

    #[test]
    fn style_lists_have_no_repeats() {
        fn distinct<T: PartialEq>(all: &[T]) -> bool {
            all.iter().enumerate().all(|(i, a)| all[..i].iter().all(|b| b != a))
        }
        assert!(distinct(&Volume::ALL.map(Volume::label)));
        assert!(distinct(&Eq::ALL.map(Eq::label)));
        assert!(distinct(&Filter::ALL.map(Filter::label)));
        assert!(distinct(&Effect::ALL.map(Effect::label)));
        assert!(distinct(&Looping::ALL.map(Looping::label)));
        assert!(distinct(&Preset::ALL.map(Preset::styles)));
    }

    #[test]
    fn smooth_crossfade_keeps_power() {
        let t = transition(Preset::Fade);
        for i in 0..=20 {
            let m = t.at(i as f32 / 20.0);
            let power = m.out.gain.powi(2) + m.inc.gain.powi(2);
            assert!((power - 1.0).abs() < 0.01, "{i}: {power}");
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
    fn filters_close_on_the_way_out_and_open_on_the_way_in() {
        let mut t = transition(Preset::Fade);
        t.set_filter(Filter::HighPassInLowPassOut);
        let (start, end) = (t.at(0.0), t.at(1.0));
        assert!(end.out.filter < -0.7, "low-pass shut on A");
        assert!(start.inc.filter > 0.7, "high-pass shut on B");
        assert!(end.inc.filter.abs() < 1e-6);
        t.set_filter(Filter::HighPassHalfOut);
        assert!((t.at(1.0).out.filter - SWEEP / 2.0).abs() < 1e-6);
        t.set_filter(Filter::NoiseOutEnd);
        assert_eq!((t.at(0.4).out.noise, t.at(1.0).out.noise, t.at(1.0).out.filter), (0.0, 1.0, 0.0));
    }

    #[test]
    fn effects_go_on_the_outgoing_song_and_ring_on_when_out() {
        let mut t = transition(Preset::Fade);
        for e in Effect::ALL {
            t.set_effect(e);
            assert_eq!(t.curve(Lane::Effect, Track::In), None, "{e:?}");
            assert_eq!(t.curve(Lane::Effect, Track::Out).is_some(), e != Effect::None);
            assert_eq!(e.rings_out(), e.label().contains(" out "), "{e:?}");
        }
        t.set_effect(Effect::ReverbOutCenter);
        assert!(t.at(0.45).out.send > 0.8 && t.at(0.55).out.send == 0.0, "the send stops at the midpoint");
        assert_eq!(Effect::DelayHalfCutEnd.kind(), Some(EffectKind::Delay { beats: 0.5 }));
        assert_eq!(Effect::EchoOneOutEnd.kind(), Some(EffectKind::Echo { beats: 1.0 }));
    }

    #[test]
    fn the_preset_is_the_one_whose_styles_it_has() {
        let mut t = transition(Preset::Blend);
        t.set_effect(Effect::EchoHalfOutEnd);
        assert_eq!(t.preset(), None);
        t.set_preset(Preset::Rise);
        assert_eq!(t.styles(), Preset::Rise.styles());
        assert_eq!(t.preset(), Some(Preset::Rise));
        t.looping = Looping::TwoBeat;
        assert_eq!(t.preset(), None);
        t.looping = Looping::None;
        assert_eq!(t.preset(), Some(Preset::Rise), "back to Rise's styles");
    }

    #[test]
    fn drawn_curves_replace_the_style_until_it_changes() {
        let mut t = transition(Preset::Fade);
        assert_eq!(t.redraw(Lane::Filter, Track::Out, |_| ()), None, "Fade has no filter");
        let len = t.redraw(Lane::Volume, Track::Out, |c| {
            c.move_point(1, 0.5, 0.0);
            c.len()
        });
        assert_eq!(len, Some(3), "starts from the style's curve");
        assert_eq!(t.preset(), None);
        assert!(t.is_custom(Lane::Volume, Track::Out) && !t.is_custom(Lane::Volume, Track::In));
        assert!(t.at(0.5).out.gain.abs() < 1e-6);
        // The incoming song's curve is untouched.
        assert!((t.at(0.5).inc.gain - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
        t.set_volume(Volume::Overlap);
        assert!(!t.is_custom(Lane::Volume, Track::Out));
        assert!(t.curves.is_empty());

        t.redraw(Lane::Eq, Track::In, |c| c.move_point(0, 0.0, 1.0));
        assert!(!t.curves.is_empty());
        t.redraw(Lane::Eq, Track::In, |c| c.move_point(0, 0.0, 0.0));
        assert!(t.curves.is_empty(), "drawn back to the style's shape");
        t.redraw(Lane::Eq, Track::In, |c| c.move_point(0, 0.0, 1.0));
        t.reset_curve(Lane::Eq, Track::In);
        assert!(t.curves.is_empty());
    }

    #[test]
    fn stores_and_reads_back_drawn_curves() {
        let mut t = transition(Preset::Surge);
        t.redraw(Lane::Filter, Track::Out, |c| c.move_point(1, c[1].x, 0.3));
        let json = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<Transition>(&json).unwrap(), t);
        let plain = serde_json::to_string(&transition(Preset::Fade)).unwrap();
        assert!(!plain.contains("curves"), "{plain}");
        // Frontends are told the preset, which reading works out again.
        let preset = |t: &Transition| serde_json::to_value(t).unwrap()["preset"].clone();
        assert_eq!(preset(&transition(Preset::Fade)), "fade");
        assert_eq!(preset(&t), serde_json::Value::Null);
    }

    #[test]
    fn reads_only_transitions_that_can_play() {
        let read = |bars: u32, curves: &str| {
            let json = format!(
                r#"{{"volume":"smooth","eq":"center_bass","filter":"high_pass_out","effect":"none","curves":{curves},"bars":{bars},"out_ms":0,"in_ms":0,"sync":true,"auto":false}}"#
            );
            serde_json::from_str::<Transition>(&json)
        };
        let ramp = r#"[{"x":0,"y":0},{"x":1,"y":0.5}]"#;
        assert!(read(8, "{}").is_ok());
        assert!(read(0, "{}").is_err() && read(3, "{}").is_err(), "lengths not offered");
        assert!(read(8, &format!(r#"{{"filter":[{ramp},null]}}"#)).is_ok(), "A has a filter to redraw");
        assert!(read(8, &format!(r#"{{"filter":[null,{ramp}]}}"#)).is_err(), "B has none");
        assert!(read(8, r#"{"volume":[[{"x":0,"y":5},{"x":1,"y":0}],null]}"#).is_err(), "a curve that isn't one");
        // Center bass swap's own curve for A, drawn by hand.
        let center = r#"[{"x":0,"y":1},{"x":0.485,"y":1},{"x":0.515,"y":0},{"x":1,"y":0}]"#;
        assert!(read(8, &format!(r#"{{"eq":[{center},null]}}"#)).unwrap().curves.is_empty(), "is the style");
    }

    #[test]
    fn reads_transitions_stored_before_filters_and_effects_split() {
        let old = |preset: &str, effect: &str| {
            let json = format!(
                r#"{{"preset":"{preset}","volume":"overlap","eq":"end_bass","effect":"{effect}","bars":8,"out_ms":1.0,"in_ms":2.0,"sync":true,"auto":false}}"#
            );
            serde_json::from_str::<Transition>(&json).unwrap()
        };
        let rise = old("rise", "filter_swap");
        assert_eq!((rise.preset(), rise.filter, rise.effect, rise.looping), (Some(Preset::Rise), Filter::LowPassInHighPassOut, Effect::None, Looping::None));
        let echo = old("custom", "filter_echo");
        assert_eq!((echo.filter, echo.effect), (Filter::HighPassOut, Effect::EchoThreeQuarterOutEnd));
        assert_eq!(old("custom", "brake").effect, Effect::None);
        // Rise's recipe has no echo, so an old Rise with one is now Custom.
        assert_eq!(old("rise", "echo").preset(), None);
        assert_eq!(old("rise", "none").filter, Filter::None);
    }

    #[test]
    fn loops_replay_the_start_of_the_window() {
        let mut t = transition(Preset::Fade);
        t.out_ms = 10_000.0;
        let g = Grid { bpm: 120.0, downbeat_ms: 0.0 };
        assert_eq!(t.loop_map(Some(&g)), None);
        t.looping = Looping::FourBeat;
        let map = t.loop_map(Some(&g)).unwrap();
        // 4 beats at 120 BPM: two seconds, replayed through the 16 s window.
        assert_eq!(map.offset(9_000.0), 0.0);
        assert_eq!(map.offset(11_000.0), 0.0);
        assert_eq!(map.offset(12_500.0), -2_000.0);
        assert!(!map.repeating(11_999.0) && map.repeating(12_000.0));
        let (now, before) = map.jump(12_010.0);
        assert_eq!((now, before), (-2_000.0, Some((0.0, 10.0))));
        assert_eq!(map.jump(13_000.0).1, Some((0.0, 1_000.0)));
        assert_eq!(map.offset(30_000.0), -20_000.0, "keeps looping past the window");
        assert_eq!(map.scaled(48.0).offset(12_500.0 * 48.0), -2_000.0 * 48.0);
    }

    #[test]
    fn loop_halving_shortens_the_loop_towards_the_end() {
        let blocks = Looping::Halving.blocks(32.0);
        assert_eq!(blocks.iter().map(|b| b.beats).collect::<Vec<_>>(), vec![4.0, 2.0, 1.0, 0.5]);
        assert_eq!((blocks[0].start, blocks[3].end), (0.0, 1.0));
        assert!(blocks.windows(2).all(|w| w[0].end == w[1].start));
        let mut t = transition(Preset::Fade);
        t.looping = Looping::Halving;
        let map = t.loop_map(Some(&Grid { bpm: 120.0, downbeat_ms: 0.0 })).unwrap();
        // The second stage starts at 8 s and replays from the window's start every second.
        assert_eq!(map.offset(8_500.0), -8_000.0);
        assert_eq!(map.offset(9_500.0), -9_000.0);
        assert_eq!(map.jump(8_001.0), (-8_000.0, Some((-6_000.0, 1.0))));
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
        assert_eq!(t.preset(), Some(Preset::Blend));
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
        assert_eq!((none.preset(), none.bars, none.sync), (Some(Preset::Fade), 4, false));
        assert_eq!(none.out_ms, 200_000.0 - 8_000.0);
    }

    #[test]
    fn auto_picks_by_energy_and_key() {
        let pick = |a: Analysis, b: Analysis| {
            let c = compat(Some(&a), Some(&b));
            let side = |f| Side { duration_ms: 200_000.0, analysis: Some(f) };
            auto_transition(side(&a), side(&b), &c).preset()
        };
        assert_eq!(pick(feat(124.0, 9, 0, 0.4), feat(125.0, 9, 0, 0.7)), Some(Preset::Rise));
        assert_eq!(pick(feat(124.0, 9, 0, 0.8), feat(125.0, 9, 0, 0.4)), Some(Preset::Melt));
        assert_eq!(pick(feat(124.0, 9, 0, 0.6), feat(125.0, 3, 0, 0.6)), Some(Preset::Wave));
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
