//! One deck: plays a decoded song from a position, through a loop if it has
//! one, time-stretched when its tempo is changed (the pitch is kept), through
//! its EQ, filter and gain, with an echo or reverb and a noise sweep on top.

use crate::dsp::{ECHO_FEEDBACK, Echo, Eq3, Filter, Noise, Reverb, Smooth};
use crate::pcm::Pcm;
use vimj_core::transition::{DeckFx, EffectKind, LoopMap};
use signalsmith_stretch::Stretch;
use std::sync::Arc;

/// Crossfade from the stretched to the plain signal once the tempo is back to normal.
const UNSTRETCH_FRAMES: usize = 1_024;
/// Tempos this close to normal play without the stretcher.
const TEMPO_EPSILON: f64 = 1e-6;
/// A loop's jumps back to its start are crossfaded over this long.
const SEAM_SECONDS: f64 = 0.004;

/// The song as a deck reads it: through the loop, if there is one.
#[derive(Clone, Copy)]
struct Source<'a> {
    pcm: &'a Pcm,
    looping: Option<&'a LoopMap>,
}

impl Source<'_> {
    #[inline]
    fn frame(&self, t: i64) -> [f32; 2] {
        if self.looping.is_none() { self.pcm.frame(t) } else { self.frame_at(t as f64) }
    }

    #[inline]
    fn frame_at(&self, t: f64) -> [f32; 2] {
        let Some(map) = self.looping else { return self.pcm.frame_at(t) };
        let (offset, jump) = map.jump(t);
        let now = self.pcm.frame_at(t + offset);
        let seam = SEAM_SECONDS * f64::from(self.pcm.rate);
        match jump {
            Some((before, since)) if since < seam => {
                let old = self.pcm.frame_at(t + before);
                let k = (since / seam) as f32;
                [old[0] + (now[0] - old[0]) * k, old[1] + (now[1] - old[1]) * k]
            }
            _ => now,
        }
    }
}

pub struct Deck {
    pcm: Arc<Pcm>,
    /// Frame of the song coming out now, on the timeline it would play
    /// without the loop.
    position: f64,
    /// Repeats part of the song, in frames.
    looping: Option<LoopMap>,
    /// Tempo relative to the song's own.
    tempo: f64,
    stretch: Option<Stretcher>,
    /// Frames left in the crossfade back to the plain signal.
    unstretch: usize,
    eq: Eq3,
    filter: Filter,
    /// What the effect send feeds.
    effect: Option<EffectKind>,
    echo: Echo,
    reverb: Reverb,
    noise: Noise,
    gain: Smooth,
    /// Loudness normalisation, linear.
    level: f32,
    /// Where the automation wants this deck.
    pub fx: DeckFx,
    scratch: Vec<f32>,
    plain: Vec<f32>,
}

impl Deck {
    pub fn new(pcm: Arc<Pcm>, position: f64, tempo: f64, level: f32) -> Deck {
        let rate = pcm.rate;
        let mut deck = Deck {
            pcm,
            position,
            looping: None,
            tempo: 1.0,
            stretch: None,
            unstretch: 0,
            eq: Eq3::new(rate),
            filter: Filter::new(rate),
            effect: None,
            echo: Echo::new(rate),
            reverb: Reverb::new(rate),
            noise: Noise::new(rate),
            gain: Smooth::new(1.0, 0.004, rate),
            level,
            fx: DeckFx::FLAT,
            scratch: Vec::new(),
            plain: Vec::new(),
        };
        deck.set_tempo(tempo);
        deck
    }

    /// Position in frames, on the timeline the song would play without the loop.
    pub fn position(&self) -> f64 {
        self.position
    }

    pub fn frames(&self) -> usize {
        self.pcm.frames()
    }

    pub fn tempo(&self) -> f64 {
        self.tempo
    }

    /// Past the end of the song.
    pub fn at_end(&self) -> bool {
        self.position >= self.pcm.frames() as f64
    }

    pub fn set_tempo(&mut self, tempo: f64) {
        let tempo = tempo.clamp(0.5, 2.0);
        if (tempo - 1.0).abs() <= TEMPO_EPSILON {
            self.tempo = 1.0;
            if self.stretch.is_some() && self.unstretch == 0 {
                self.unstretch = UNSTRETCH_FRAMES;
                self.position = self.position.round();
            }
            return;
        }
        self.tempo = tempo;
        self.unstretch = 0;
        if self.stretch.is_none() {
            let source = Source { pcm: &self.pcm, looping: self.looping.as_ref() };
            self.stretch = Some(Stretcher::new(source, self.position, tempo));
        }
    }

    /// Loops part of the song from now on; `map` is in frames.
    pub fn set_loop(&mut self, map: Option<LoopMap>) {
        self.looping = map;
    }

    /// Sets what the effect send feeds; echoes and delays repeat in beats of `beat_frames`.
    pub fn set_effect(&mut self, effect: Option<EffectKind>, beat_frames: f64) {
        self.effect = effect;
        let (beats, feedback) = match effect {
            Some(EffectKind::Echo { beats }) => (beats, ECHO_FEEDBACK),
            Some(EffectKind::Delay { beats }) => (beats, 0.0),
            Some(EffectKind::Reverb) | None => return,
        };
        self.echo.set_delay((f64::from(beats) * beat_frames).round() as usize);
        self.echo.set_feedback(feedback);
    }

    /// Silenced and done ringing.
    pub fn silent(&self) -> bool {
        self.fx.gain == 0.0 && self.gain.value() < 1e-4 && !self.echo.ringing() && !self.reverb.ringing() && !self.noise.sounding()
    }

    /// Fades the deck out; its effect, if any, rings on.
    pub fn retire(&mut self) {
        self.fx.gain = 0.0;
        self.fx.send = 0.0;
        self.fx.noise = 0.0;
    }

    /// Stops the effect ringing on.
    pub fn cut_tail(&mut self) {
        self.echo.cut();
        self.reverb.cut();
    }

    /// Adds `out.len() / 2` frames of this deck to `out` (interleaved stereo).
    pub fn render(&mut self, out: &mut [f32]) {
        let n = out.len() / 2;
        self.source(n);
        let fx = self.fx;
        let (echo, reverb) = match self.effect {
            Some(EffectKind::Reverb) => (0.0, fx.send),
            Some(_) => (fx.send, 0.0),
            None => (0.0, 0.0),
        };
        for (i, frame) in out.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let x = [self.scratch[i * 2] * self.level, self.scratch[i * 2 + 1] * self.level];
            let shaped = self.filter.process(self.eq.process(x, fx.eq), fx.filter);
            let gain = self.gain.next(fx.gain);
            let dry = [shaped[0] * gain, shaped[1] * gain];
            // Sends take the song before its fader, so an effect can carry it out.
            let y = self.echo.process(dry, shaped, echo);
            let y = self.reverb.process(y, shaped, reverb);
            let noise = self.noise.process(fx.noise);
            frame[0] += y[0] + noise[0];
            frame[1] += y[1] + noise[1];
        }
    }

    /// Fills `scratch` with the next `n` frames of the song itself.
    fn source(&mut self, n: usize) {
        self.scratch.resize(n * 2, 0.0);
        let source = Source { pcm: &self.pcm, looping: self.looping.as_ref() };
        let Some(stretch) = self.stretch.as_mut() else {
            for i in 0..n {
                let f = if self.position.fract() == 0.0 { source.frame(self.position as i64) } else { source.frame_at(self.position) };
                self.scratch[i * 2] = f[0];
                self.scratch[i * 2 + 1] = f[1];
                self.position += 1.0;
            }
            return;
        };
        stretch.render(source, self.tempo, n, &mut self.scratch);
        if self.unstretch == 0 {
            self.position += n as f64 * self.tempo;
            return;
        }
        // Blend into the plain signal, which is time-aligned with the stretched one.
        self.plain.clear();
        for i in 0..n {
            self.plain.extend(source.frame(self.position as i64 + i as i64));
        }
        for i in 0..n {
            let t = (UNSTRETCH_FRAMES - self.unstretch.saturating_sub(i)) as f32 / UNSTRETCH_FRAMES as f32;
            for c in 0..2 {
                let k = i * 2 + c;
                self.scratch[k] += (self.plain[k] - self.scratch[k]) * t.min(1.0);
            }
        }
        self.position += n as f64;
        self.unstretch = self.unstretch.saturating_sub(n);
        if self.unstretch == 0 {
            self.stretch = None;
        }
    }
}

/// Signalsmith Stretch, fed so that its output lines up with `Deck::position`.
struct Stretcher {
    inner: Stretch,
    /// Next source frame to feed in.
    feed: i64,
    /// Fractional input frames owed from earlier blocks.
    carry: f64,
    input: Vec<f32>,
}

impl Stretcher {
    fn new(source: Source, position: f64, tempo: f64) -> Stretcher {
        let mut inner = Stretch::preset_default(2, source.pcm.rate);
        let input_latency = inner.input_latency();
        // After seeking, the stretcher is `input_latency` behind its input and
        // its output trails by `output_latency` more, so feed that far ahead.
        let feed = (position + input_latency as f64 + inner.output_latency() as f64 * tempo).round() as i64;
        let preroll = 4 * input_latency as i64;
        let mut input = Vec::with_capacity(preroll as usize * 2);
        for f in feed - preroll..feed {
            input.extend(source.frame(f));
        }
        inner.seek(&input, tempo);
        Stretcher { inner, feed, carry: 0.0, input }
    }

    fn render(&mut self, source: Source, tempo: f64, n: usize, out: &mut [f32]) {
        let exact = self.carry + n as f64 * tempo;
        let take = exact.floor() as usize;
        self.carry = exact - take as f64;
        self.input.clear();
        for f in self.feed..self.feed + take as i64 {
            self.input.extend(source.frame(f));
        }
        self.feed += take as i64;
        self.inner.process(&self.input, &mut out[..n * 2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// One-sample clicks every `spacing` frames.
    fn clicks(spacing: usize, count: usize) -> Arc<Pcm> {
        let mut s = vec![0.0f32; spacing * count * 2];
        for k in 0..count {
            // A short burst rather than one sample, so the stretcher keeps it.
            for j in 0..48 {
                let v = (j as f32 * 0.8).sin() * (1.0 - j as f32 / 48.0);
                s[(k * spacing + j) * 2] = v;
                s[(k * spacing + j) * 2 + 1] = v;
            }
        }
        Arc::new(Pcm::from_f32(RATE, &s))
    }

    fn render(deck: &mut Deck, frames: usize) -> Vec<f32> {
        let mut out = vec![0.0; frames * 2];
        for block in out.chunks_mut(512) {
            deck.render(block);
        }
        out.as_chunks::<2>().0.iter().map(|f| f[0]).collect()
    }

    /// Frames where the level first rises past half the peak, after each quiet stretch.
    fn onsets(signal: &[f32]) -> Vec<usize> {
        let peak = signal.iter().fold(0.0f32, |m, x| m.max(x.abs()));
        let mut found = Vec::new();
        let mut quiet = usize::MAX;
        for (i, x) in signal.iter().enumerate() {
            if x.abs() > peak * 0.5 {
                if quiet > 2_000 {
                    found.push(i);
                }
                quiet = 0;
            } else {
                quiet = quiet.saturating_add(1);
            }
        }
        found
    }

    #[test]
    fn plays_from_a_position() {
        let mut deck = Deck::new(clicks(24_000, 10), 12_000.0, 1.0, 1.0);
        let got = onsets(&render(&mut deck, 96_000));
        // The burst passes half its peak a few samples in.
        assert_eq!(got, vec![12_004, 36_004, 60_004, 84_004]);
        assert_eq!(deck.position(), 108_000.0);
    }

    #[test]
    fn stretched_output_lines_up_with_the_position() {
        let pcm = clicks(24_000, 20);
        for tempo in [1.0001, 1.05, 0.95] {
            let mut deck = Deck::new(pcm.clone(), 12_000.0, tempo, 1.0);
            let got = onsets(&render(&mut deck, 200_000));
            assert!(got.len() >= 6, "{tempo}: {got:?}");
            for (k, &frame) in got.iter().enumerate() {
                let expected = (12_000.0 + 24_000.0 * k as f64) / tempo;
                eprintln!("tempo {tempo}, click {k}: off by {:.0}", frame as f64 - expected);
                assert!((frame as f64 - expected).abs() < 240.0, "tempo {tempo}, click {k}: {frame} vs {expected:.0}");
            }
        }
    }

    #[test]
    fn returns_to_the_plain_signal_without_jumping() {
        let pcm = clicks(24_000, 20);
        let mut deck = Deck::new(pcm, 0.0, 1.03, 1.0);
        render(&mut deck, 48_000);
        deck.set_tempo(1.0);
        let start = deck.position();
        let got = onsets(&render(&mut deck, 96_000));
        assert!(deck.stretch.is_none());
        // Next click in the source is at the next multiple of 24 000.
        let next = (start / 24_000.0).ceil() * 24_000.0 - start;
        assert!((got[0] as f64 - next).abs() < 240.0, "{got:?} vs {next}");
        assert_eq!(deck.position(), start.round() + 96_000.0);
    }

    #[test]
    fn loops_replay_part_of_the_song() {
        let mut deck = Deck::new(clicks(24_000, 10), 0.0, 1.0, 1.0);
        // From 12 000, replay 18 000 frames, which hold the click at 24 000.
        let stage = vimj_core::transition::Stage { from: 12_000.0, to: 1e9, period: 18_000.0 };
        deck.set_loop(Some(LoopMap { start: 12_000.0, stages: vec![stage] }));
        let got = onsets(&render(&mut deck, 90_000));
        assert_eq!(got, vec![4, 24_004, 42_004, 60_004, 78_004]);
        assert_eq!(deck.position(), 90_000.0, "the position runs on as if unlooped");
    }

    #[test]
    fn effects_ring_on_until_cut() {
        for effect in [EffectKind::Reverb, EffectKind::Echo { beats: 0.5 }] {
            let mut deck = Deck::new(clicks(4_800, 100), 0.0, 1.0, 1.0);
            deck.set_effect(Some(effect), 12_000.0);
            deck.fx.send = 1.0;
            render(&mut deck, 24_000);
            deck.retire();
            render(&mut deck, 4_800);
            assert!(!deck.silent(), "{effect:?} rings on");
            deck.cut_tail();
            render(&mut deck, 4_800);
            assert!(deck.silent(), "{effect:?} cut");
        }
    }

    #[test]
    fn retired_decks_fall_silent() {
        let mut deck = Deck::new(clicks(4_800, 100), 0.0, 1.0, 1.0);
        deck.retire();
        render(&mut deck, 4_800);
        assert!(deck.silent());
    }
}
