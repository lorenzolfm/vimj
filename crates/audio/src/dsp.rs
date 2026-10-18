//! Building blocks for the decks: parameter smoothing, filters, a three-band
//! isolator EQ, a DJ-style filter knob, an echo and the master clipper.

use std::f32::consts::PI;

/// One-pole smoothing towards a target, so parameter changes don't click.
#[derive(Clone, Copy, Debug)]
pub struct Smooth {
    value: f32,
    coef: f32,
}

impl Smooth {
    pub fn new(value: f32, seconds: f32, rate: u32) -> Smooth {
        Smooth { value, coef: 1.0 - (-1.0 / (seconds * rate as f32)).exp() }
    }

    #[inline]
    pub fn next(&mut self, target: f32) -> f32 {
        self.value += (target - self.value) * self.coef;
        self.value
    }

    pub fn value(&self) -> f32 {
        self.value
    }

    pub fn set(&mut self, value: f32) {
        self.value = value;
    }
}

/// Second-order section, transposed direct form II, with stereo state.
#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: [f32; 2],
    z2: [f32; 2],
}

impl Biquad {
    /// RBJ cookbook low-pass.
    pub fn lowpass(freq: f32, q: f32, rate: u32) -> Biquad {
        let (cos, alpha) = Self::prewarp(freq, q, rate);
        let a0 = 1.0 + alpha;
        Self::normalised((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0, a0, -2.0 * cos, 1.0 - alpha)
    }

    /// RBJ cookbook high-pass.
    pub fn highpass(freq: f32, q: f32, rate: u32) -> Biquad {
        let (cos, alpha) = Self::prewarp(freq, q, rate);
        let a0 = 1.0 + alpha;
        Self::normalised((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, a0, -2.0 * cos, 1.0 - alpha)
    }

    /// RBJ cookbook all-pass.
    pub fn allpass(freq: f32, q: f32, rate: u32) -> Biquad {
        let (cos, alpha) = Self::prewarp(freq, q, rate);
        Self::normalised(1.0 - alpha, -2.0 * cos, 1.0 + alpha, 1.0 + alpha, -2.0 * cos, 1.0 - alpha)
    }

    fn prewarp(freq: f32, q: f32, rate: u32) -> (f32, f32) {
        let w = 2.0 * PI * freq / rate as f32;
        (w.cos(), w.sin() / (2.0 * q))
    }

    fn normalised(b0: f32, b1: f32, b2: f32, a0: f32, a1: f32, a2: f32) -> Biquad {
        Biquad { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0, z1: [0.0; 2], z2: [0.0; 2] }
    }

    #[inline]
    pub fn process(&mut self, channel: usize, x: f32) -> f32 {
        let y = self.b0 * x + self.z1[channel];
        self.z1[channel] = self.b1 * x - self.a1 * y + self.z2[channel];
        self.z2[channel] = self.b2 * x - self.a2 * y;
        y
    }
}

const BUTTERWORTH_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;
pub const LOW_CROSSOVER_HZ: f32 = 250.0;
pub const HIGH_CROSSOVER_HZ: f32 = 3_000.0;

/// Three-band isolator built from Linkwitz–Riley crossovers. The low band
/// goes through the high crossover's all-pass so all three stay in phase: at
/// unity gains the bands sum to a flat response, and a gain of 0 removes a band.
pub struct Eq3 {
    low: [Biquad; 2],
    rest: [Biquad; 2],
    low_align: Biquad,
    mid: [Biquad; 2],
    high: [Biquad; 2],
    gains: [Smooth; 3],
}

impl Eq3 {
    pub fn new(rate: u32) -> Eq3 {
        let q = BUTTERWORTH_Q;
        Eq3 {
            low: [Biquad::lowpass(LOW_CROSSOVER_HZ, q, rate); 2],
            rest: [Biquad::highpass(LOW_CROSSOVER_HZ, q, rate); 2],
            low_align: Biquad::allpass(HIGH_CROSSOVER_HZ, q, rate),
            mid: [Biquad::lowpass(HIGH_CROSSOVER_HZ, q, rate); 2],
            high: [Biquad::highpass(HIGH_CROSSOVER_HZ, q, rate); 2],
            gains: [Smooth::new(1.0, 0.01, rate); 3],
        }
    }

    #[inline]
    pub fn process(&mut self, frame: [f32; 2], target: [f32; 3]) -> [f32; 2] {
        let g = [self.gains[0].next(target[0]), self.gains[1].next(target[1]), self.gains[2].next(target[2])];
        let mut out = [0.0; 2];
        for (c, &x) in frame.iter().enumerate() {
            let low = cascade(&mut self.low, c, x);
            let low = self.low_align.process(c, low);
            let rest = cascade(&mut self.rest, c, x);
            let mid = cascade(&mut self.mid, c, rest);
            let high = cascade(&mut self.high, c, rest);
            out[c] = g[0] * low + g[1] * mid + g[2] * high;
        }
        out
    }
}

#[inline]
fn cascade(stages: &mut [Biquad; 2], channel: usize, x: f32) -> f32 {
    let y = stages[0].process(channel, x);
    stages[1].process(channel, y)
}

/// State-variable filter (Simper's trapezoidal form), stereo.
#[derive(Clone, Copy, Debug, Default)]
struct Svf {
    ic1: [f32; 2],
    ic2: [f32; 2],
    g: f32,
    k: f32,
}

impl Svf {
    fn tune(&mut self, cutoff: f32, q: f32, rate: u32) {
        let cutoff = cutoff.clamp(10.0, rate as f32 * 0.45);
        self.g = (PI * cutoff / rate as f32).tan();
        self.k = 1.0 / q;
    }

    /// Returns (low-pass, high-pass).
    #[inline]
    fn process(&mut self, c: usize, x: f32) -> (f32, f32) {
        let a1 = 1.0 / (1.0 + self.g * (self.g + self.k));
        let a2 = self.g * a1;
        let v3 = x - self.ic2[c];
        let v1 = a1 * self.ic1[c] + a2 * v3;
        let v2 = self.ic2[c] + self.g * v1;
        self.ic1[c] = 2.0 * v1 - self.ic1[c];
        self.ic2[c] = 2.0 * v2 - self.ic2[c];
        (v2, x - self.k * v1 - v2)
    }
}

const FILTER_Q: f32 = 0.9;
/// Retuning the filter costs a `tan`, so do it every few samples.
const RETUNE_EVERY: u32 = 16;
/// Below this knob position the dry signal is blended back in, so the open filter is transparent.
const ENGAGE: f32 = 0.05;

/// A mixer's filter knob: left of centre closes a low-pass, right of centre
/// opens a high-pass.
pub struct Filter {
    lp: Svf,
    hp: Svf,
    knob: Smooth,
    rate: u32,
    counter: u32,
}

impl Filter {
    pub fn new(rate: u32) -> Filter {
        let mut f = Filter { lp: Svf::default(), hp: Svf::default(), knob: Smooth::new(0.0, 0.02, rate), rate, counter: 0 };
        f.retune(0.0);
        f
    }

    /// Low-pass cutoff and high-pass cutoff for a knob position.
    pub fn cutoffs(knob: f32) -> (f32, f32) {
        let lp = if knob < 0.0 { 20_000.0 * (150.0f32 / 20_000.0).powf(-knob) } else { 20_000.0 };
        let hp = if knob > 0.0 { 20.0 * (4_000.0f32 / 20.0).powf(knob) } else { 20.0 };
        (lp, hp)
    }

    fn retune(&mut self, knob: f32) {
        let (lp, hp) = Self::cutoffs(knob);
        self.lp.tune(lp, FILTER_Q, self.rate);
        self.hp.tune(hp, FILTER_Q, self.rate);
    }

    #[inline]
    pub fn process(&mut self, frame: [f32; 2], target: f32) -> [f32; 2] {
        let knob = self.knob.next(target);
        if self.counter == 0 {
            self.retune(knob);
        }
        self.counter = (self.counter + 1) % RETUNE_EVERY;
        let wet = (knob.abs() / ENGAGE).min(1.0);
        let mut out = [0.0; 2];
        for (c, &x) in frame.iter().enumerate() {
            let (low, _) = self.lp.process(c, x);
            let (_, high) = self.hp.process(c, low);
            out[c] = x + (high - x) * wet;
        }
        out
    }
}

/// A feedback delay, timed to the beat, whose tail rings on after its input stops.
pub struct Echo {
    buffer: Vec<[f32; 2]>,
    write: usize,
    delay: usize,
    feedback: f32,
    send: Smooth,
    /// Peak level in the delay line recently, to know when the tail has died.
    level: f32,
}

impl Echo {
    pub fn new(rate: u32) -> Echo {
        Echo {
            buffer: vec![[0.0; 2]; rate as usize * 2],
            write: 0,
            delay: rate as usize / 2,
            feedback: 0.55,
            send: Smooth::new(0.0, 0.01, rate),
            level: 0.0,
        }
    }

    pub fn set_delay(&mut self, frames: usize) {
        self.delay = frames.clamp(1, self.buffer.len() - 1);
    }

    /// Adds the echo of `input` (the send) to `dry`.
    #[inline]
    pub fn process(&mut self, dry: [f32; 2], input: [f32; 2], send_target: f32) -> [f32; 2] {
        let send = self.send.next(send_target);
        if send < 1e-4 && self.level < 1e-4 {
            self.level = 0.0;
            return dry;
        }
        let len = self.buffer.len();
        let read = (self.write + len - self.delay) % len;
        let wet = self.buffer[read];
        self.buffer[self.write] = [input[0] * send + wet[0] * self.feedback, input[1] * send + wet[1] * self.feedback];
        self.write = (self.write + 1) % len;
        self.level = (self.level * 0.9999).max(wet[0].abs().max(wet[1].abs()));
        [dry[0] + wet[0], dry[1] + wet[1]]
    }

    pub fn ringing(&self) -> bool {
        self.level >= 1e-4
    }
}

/// Transparent below the knee, then rounds peaks off so the output never passes full scale.
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    const KNEE: f32 = 0.85;
    let a = x.abs();
    if a <= KNEE {
        x
    } else {
        let over = (a - KNEE) / (1.0 - KNEE);
        x.signum() * (KNEE + (1.0 - KNEE) * over.tanh())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    fn sine(freq: f32, frames: usize) -> Vec<f32> {
        (0..frames).map(|i| (2.0 * PI * freq * i as f32 / RATE as f32).sin()).collect()
    }

    /// RMS of the second half, after filters settle.
    fn rms_tail(xs: &[f32]) -> f32 {
        let tail = &xs[xs.len() / 2..];
        (tail.iter().map(|x| x * x).sum::<f32>() / tail.len() as f32).sqrt()
    }

    fn through_eq(freq: f32, gains: [f32; 3]) -> f32 {
        let mut eq = Eq3::new(RATE);
        let out: Vec<f32> = sine(freq, RATE as usize / 2).iter().map(|&x| eq.process([x, x], gains)[0]).collect();
        rms_tail(&out) / std::f32::consts::FRAC_1_SQRT_2
    }

    #[test]
    fn eq_is_flat_at_unity() {
        for freq in [40.0, 250.0, 1_000.0, 3_000.0, 12_000.0] {
            let gain = through_eq(freq, [1.0; 3]);
            assert!((gain - 1.0).abs() < 0.02, "{freq} Hz: {gain}");
        }
    }

    #[test]
    fn eq_kills_bands() {
        assert!(through_eq(60.0, [0.0, 1.0, 1.0]) < 0.1, "low kill");
        assert!(through_eq(60.0, [1.0, 0.0, 0.0]) > 0.9, "lows survive mid/high kill");
        assert!(through_eq(8_000.0, [1.0, 1.0, 0.0]) < 0.1, "high kill");
        assert!(through_eq(1_000.0, [1.0, 0.0, 1.0]) < 0.2, "mid kill");
        assert!(through_eq(8_000.0, [0.0, 0.0, 1.0]) > 0.9, "highs survive low/mid kill");
    }

    #[test]
    fn filter_knob_sweeps_both_ways() {
        let run = |freq: f32, knob: f32| {
            let mut f = Filter::new(RATE);
            let out: Vec<f32> = sine(freq, RATE as usize / 2).iter().map(|&x| f.process([x, x], knob)[0]).collect();
            rms_tail(&out) / std::f32::consts::FRAC_1_SQRT_2
        };
        assert!((run(1_000.0, 0.0) - 1.0).abs() < 1e-3, "open is transparent");
        assert!(run(8_000.0, -0.8) < 0.05, "low-pass closes on highs");
        assert!(run(60.0, -0.8) > 0.9, "low-pass keeps lows");
        assert!(run(60.0, 0.8) < 0.05, "high-pass removes lows");
        assert!(run(8_000.0, 0.8) > 0.9, "high-pass keeps highs");
    }

    #[test]
    fn echo_repeats_on_the_beat_and_dies_away() {
        let mut echo = Echo::new(RATE);
        echo.set_delay(1_000);
        let mut out = Vec::new();
        for i in 0..20_000 {
            let x = if i == 0 { 1.0 } else { 0.0 };
            out.push(echo.process([0.0; 2], [x; 2], 1.0)[0]);
        }
        assert!(out[1_000] > 0.0 && out[999] == 0.0);
        assert!((out[2_000] / out[1_000] - 0.55).abs() < 0.05);
        let mut silent = Echo::new(RATE);
        assert!(!silent.ringing());
        assert_eq!(silent.process([0.3; 2], [0.3; 2], 0.0), [0.3; 2]);
    }

    #[test]
    fn soft_clip_is_bounded_and_transparent_below_the_knee() {
        assert_eq!(soft_clip(0.5), 0.5);
        assert!(soft_clip(3.0) <= 1.0 && soft_clip(3.0) > 0.99);
        assert!(soft_clip(-3.0) >= -1.0);
        assert!(soft_clip(0.9) < 0.9 && soft_clip(0.9) > 0.85);
    }
}
