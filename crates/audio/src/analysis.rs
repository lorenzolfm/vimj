//! Musical analysis of a decoded song: tempo and beat grid with downbeats,
//! key, energy, loudness, where the music starts and ends, and a waveform
//! for display.
//!
//! Tempo comes from the autocorrelation of a spectral-flux onset envelope,
//! refined by how tightly onsets cluster on a beat phase. Downbeats are where
//! the sound changes most from one bar to the next. Key comes from a
//! chromagram correlated with major and minor key profiles.

use crate::pcm::Pcm;
use dejay_core::model::{Analysis, Grid, Key};
use dejay_core::protocol::WAVEFORM_HZ;
use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

pub const VERSION: u32 = 2;
/// Bytes per waveform column: peak, low, mid, high.
pub const WAVEFORM_STRIDE: usize = 4;

const FFT: usize = 2048;
const KEY_FFT: usize = 8192;
const MIN_BPM: f64 = 60.0;
/// Shorter clips don't show a tempo reliably.
const MIN_GRID_SECONDS: f64 = 15.0;
const MAX_BPM: f64 = 200.0;
/// How sharply onsets must cluster on the beat (peak over mean of the phase
/// histogram) before the song counts as having a beat grid.
const MIN_BEAT_STRENGTH: f32 = 2.0;
/// Onset envelope peaks come this much before the onsets they mark
/// (measured on synthetic kicks).
const ONSET_LEAD_MS: f64 = 9.0;
/// Music starts and ends where the level rises above this, relative to the loudest moment.
const SILENCE_DB: f32 = -40.0;
const LOG_BANDS: usize = 24;

pub struct Output {
    pub analysis: Analysis,
    /// `WAVEFORM_STRIDE` bytes per column, `WAVEFORM_HZ` columns per second.
    pub waveform: Vec<u8>,
}

pub fn analyze(pcm: &Pcm) -> Output {
    let mono = pcm.mono();
    let frames = Frames::compute(&mono, pcm.rate);
    let onsets = onset_envelope(&frames.flux);
    let kicks = onset_envelope(&frames.low_flux);

    let (start_ms, end_ms) = music_bounds(&frames);
    let beat = beat_grid(&onsets, &kicks, frames.fps);
    let grid = beat.map(|(bpm, first_beat_ms)| {
        let beat_ms = 60_000.0 / bpm;
        let phase = downbeat_phase(&frames, first_beat_ms, beat_ms, start_ms);
        Grid { bpm, downbeat_ms: first_beat_ms + f64::from(phase) * beat_ms }
    });
    let key = detect_key(&mono, pcm.rate);
    let loudness = loudness(pcm);
    let energy = energy(&frames, &onsets, loudness, start_ms, end_ms);

    Output {
        analysis: Analysis { version: VERSION, grid, key, energy, loudness, start_ms, end_ms },
        waveform: waveform(&frames),
    }
}

/// Per-hop measurements, one every 1/`WAVEFORM_HZ` seconds.
struct Frames {
    fps: f64,
    flux: Vec<f32>,
    /// Flux below 150 Hz: kick drums, which usually sit on the beat.
    low_flux: Vec<f32>,
    /// Low, mid and high magnitude.
    bands: Vec<[f32; 3]>,
    /// Log magnitude in log-spaced bands, for comparing beats and bars.
    spectrum: Vec<[f32; LOG_BANDS]>,
    centroid: Vec<f32>,
    peak: Vec<f32>,
    rms: Vec<f32>,
}

impl Frames {
    fn compute(mono: &[f32], rate: u32) -> Frames {
        let hop = (f64::from(rate) / WAVEFORM_HZ).round() as usize;
        let fps = f64::from(rate) / hop as f64;
        let count = mono.len().div_ceil(hop);
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FFT);
        let window = hann(FFT);
        let mut input = fft.make_input_vec();
        let mut output = fft.make_output_vec();
        let bins = FFT / 2 + 1;
        let bin_hz = rate as f32 / FFT as f32;
        let bin_of = |hz: f32| ((hz / bin_hz).round() as usize).min(bins - 1);
        let (kick_end, low_end, mid_end, flux_end, top) =
            (bin_of(150.0), bin_of(200.0), bin_of(2_500.0), bin_of(10_000.0), bin_of(16_000.0));
        let band_edges: Vec<usize> = (0..=LOG_BANDS)
            .map(|b| bin_of(40.0 * (16_000.0f32 / 40.0).powf(b as f32 / LOG_BANDS as f32)).max(1))
            .collect();

        let mut f = Frames {
            fps,
            flux: Vec::with_capacity(count),
            low_flux: Vec::with_capacity(count),
            bands: Vec::with_capacity(count),
            spectrum: Vec::with_capacity(count),
            centroid: Vec::with_capacity(count),
            peak: Vec::with_capacity(count),
            rms: Vec::with_capacity(count),
        };
        let mut previous = vec![0.0f32; bins];
        let mut log_mag = vec![0.0f32; bins];
        let mut mag = vec![0.0f32; bins];
        for i in 0..count {
            let centre = i * hop;
            for (j, slot) in input.iter_mut().enumerate() {
                let at = (centre + j) as isize - (FFT / 2) as isize;
                *slot = if at >= 0 && (at as usize) < mono.len() { mono[at as usize] * window[j] } else { 0.0 };
            }
            fft.process(&mut input, &mut output).expect("buffer sizes match the plan");
            for (k, c) in output.iter().enumerate() {
                mag[k] = c.norm() * (4.0 / FFT as f32);
                log_mag[k] = (1.0 + 100.0 * mag[k]).ln();
            }
            let rise = |k: usize| (log_mag[k] - previous[k]).max(0.0);
            f.flux.push((1..flux_end).map(rise).sum());
            f.low_flux.push((1..kick_end).map(rise).sum());
            std::mem::swap(&mut previous, &mut log_mag);

            let energy = |range: std::ops::Range<usize>| mag[range].iter().map(|m| m * m).sum::<f32>().sqrt();
            f.bands.push([energy(1..low_end), energy(low_end..mid_end), energy(mid_end..top)]);
            let mut spectrum = [0.0; LOG_BANDS];
            for (b, slot) in spectrum.iter_mut().enumerate() {
                let (lo, hi) = (band_edges[b], band_edges[b + 1].max(band_edges[b] + 1));
                *slot = (1.0 + 100.0 * energy(lo..hi)).ln();
            }
            f.spectrum.push(spectrum);
            let total: f32 = mag[1..].iter().sum();
            let weighted: f32 = mag.iter().enumerate().skip(1).map(|(k, m)| k as f32 * bin_hz * m).sum();
            f.centroid.push(if total > 1e-6 { weighted / total } else { 0.0 });

            let chunk = &mono[centre.min(mono.len())..(centre + hop).min(mono.len())];
            f.peak.push(chunk.iter().fold(0.0f32, |m, x| m.max(x.abs())));
            let sum_sq: f32 = chunk.iter().map(|x| x * x).sum();
            f.rms.push(if chunk.is_empty() { 0.0 } else { (sum_sq / chunk.len() as f32).sqrt() });
        }
        f
    }
}

fn hann(n: usize) -> Vec<f32> {
    (0..n).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos()).collect()
}

/// Spectral flux with its local average removed, so only sudden rises remain.
fn onset_envelope(flux: &[f32]) -> Vec<f32> {
    const HALF: usize = 15;
    let mut prefix = vec![0.0f64; flux.len() + 1];
    for (i, &x) in flux.iter().enumerate() {
        prefix[i + 1] = prefix[i] + f64::from(x);
    }
    (0..flux.len())
        .map(|i| {
            let (lo, hi) = (i.saturating_sub(HALF), (i + HALF + 1).min(flux.len()));
            let mean = (prefix[hi] - prefix[lo]) / (hi - lo) as f64;
            (flux[i] - mean as f32).max(0.0)
        })
        .collect()
}

/// Below this phase-histogram strength the low band doesn't mark the beat clearly enough to help place it.
const MIN_KICK_STRENGTH: f32 = 1.5;

/// Tempo and the time of the first beat, in ms, or `None` without a steady
/// beat. The tempo comes from all onsets; the beat's phase from the kick
/// drum when there is one, since hats and snares often sit off the beat.
fn beat_grid(onsets: &[f32], kicks: &[f32], fps: f64) -> Option<(f64, f64)> {
    let rough = rough_tempo(onsets, fps)?;
    // Refine within ±2% in 0.005 BPM steps by how tightly onsets line up.
    let steps = (rough * 0.02 / 0.005).round() as i64;
    let mut best = (rough, 0.0f64);
    for s in -steps..=steps {
        let bpm = rough + s as f64 * 0.005;
        let tightness = tightness(onsets, fps * 60.0 / bpm);
        if tightness > best.1 {
            best = (bpm, tightness);
        }
    }
    // Most music is made at a whole-number tempo; prefer it when it fits about as well.
    let whole = best.0.round();
    if (whole - best.0).abs() < 0.2 && tightness(onsets, fps * 60.0 / whole) >= best.1 * 0.99 {
        best.0 = whole;
    }
    let best = (best.0, phase_histogram(onsets, fps * 60.0 / best.0, 48).0);
    if best.1 < MIN_BEAT_STRENGTH {
        return None;
    }
    let period = fps * 60.0 / best.0;
    let all = folded(onsets, period, 240);
    let low = folded(kicks, period, 240);
    // A beat with a kick on it shows in both the low band and the full
    // spectrum; off-beat bass shows only low, off-beat hats only high.
    let phase = if histogram_peak(&low).0 >= MIN_KICK_STRENGTH {
        let both: Vec<f32> = low.iter().zip(&all).map(|(l, a)| l * a).collect();
        histogram_peak(&both).1
    } else {
        histogram_peak(&all).1
    };
    let first_beat_ms = phase * period / fps * 1000.0 + ONSET_LEAD_MS;
    let beat_ms = 60_000.0 / best.0;
    Some((best.0, first_beat_ms.rem_euclid(beat_ms)))
}

/// How tightly onsets cluster on one phase of `period`: the length of their
/// mean phase vector, plus that of the half-period so off-beats don't cancel beats.
fn tightness(onsets: &[f32], period: f64) -> f64 {
    let (mut c1, mut s1, mut c2, mut s2, mut total) = (0.0, 0.0, 0.0, 0.0, 0.0);
    let step = std::f64::consts::TAU / period;
    for (i, &o) in onsets.iter().enumerate() {
        if o > 0.0 {
            let (o, angle) = (f64::from(o), i as f64 * step);
            let (sin, cos) = angle.sin_cos();
            c1 += o * cos;
            s1 += o * sin;
            // Double angle from the single one, saving a second sin_cos.
            c2 += o * (cos * cos - sin * sin);
            s2 += o * 2.0 * sin * cos;
            total += o;
        }
    }
    if total <= 0.0 { 0.0 } else { (c1.hypot(s1) + c2.hypot(s2)) / total }
}

/// Local maxima of the tempo score, best first, for diagnosing analysis.
#[doc(hidden)]
pub fn tempo_candidates(pcm: &Pcm) -> Vec<(f64, f64)> {
    let frames = Frames::compute(&pcm.mono(), pcm.rate);
    let onsets = onset_envelope(&frames.flux);
    let mut found = tempo_scores(&onsets, frames.fps);
    let peaks: Vec<(f64, f64)> = (1..found.len().saturating_sub(1))
        .filter(|&i| found[i].1 >= found[i - 1].1 && found[i].1 >= found[i + 1].1)
        .map(|i| found[i])
        .collect();
    found = peaks;
    found.sort_by(|a, b| b.1.total_cmp(&a.1));
    found
}

/// How strongly the kick and all onsets mark the beat at `bpm`, and where
/// each puts the first beat (ms), for diagnosing analysis.
#[doc(hidden)]
pub fn phase_report(pcm: &Pcm, bpm: f64) -> [(f32, f64); 2] {
    let frames = Frames::compute(&pcm.mono(), pcm.rate);
    let period = frames.fps * 60.0 / bpm;
    [onset_envelope(&frames.low_flux), onset_envelope(&frames.flux)].map(|o| {
        let (strength, phase) = phase_histogram(&o, period, 240);
        (strength, phase * period / frames.fps * 1000.0 + ONSET_LEAD_MS)
    })
}

/// Tempo score for every whole-frame lag between 60 and 200 BPM, as (BPM, score).
fn tempo_scores(onsets: &[f32], fps: f64) -> Vec<(f64, f64)> {
    let min_lag = (fps * 60.0 / MAX_BPM).floor() as usize;
    let max_lag = (fps * 60.0 / MIN_BPM).ceil() as usize;
    if onsets.len() < (max_lag * 4).max((fps * MIN_GRID_SECONDS) as usize) {
        return Vec::new();
    }
    let acf = |lag: usize| -> f64 {
        let n = onsets.len() - lag;
        (0..n).map(|i| f64::from(onsets[i]) * f64::from(onsets[i + lag])).sum::<f64>() / n as f64
    };
    let zero = acf(0);
    if zero <= 0.0 {
        return Vec::new();
    }
    let half_min = min_lag / 2;
    let acfs: Vec<f64> = (0..=max_lag * 2).map(|lag| if lag < half_min { 0.0 } else { acf(lag) / zero }).collect();
    // Autocorrelation at a fractional lag.
    let at = |lag: f64| {
        let (lo, t) = (lag.floor() as usize, lag.fract());
        acfs[lo] * (1.0 - t) + acfs[(lo + 1).min(acfs.len() - 1)] * t
    };
    (min_lag..=max_lag)
        .map(|lag| {
            let bpm = fps * 60.0 / lag as f64;
            let prior = (-0.5 * ((bpm / 120.0).log2() / 0.9).powi(2)).exp();
            // A real beat also repeats every two beats and has a pulse on the half beat;
            // syncopated rhythms that fake a faster tempo usually lack the half-beat pulse.
            let metre = acfs[lag] + 0.5 * acfs[lag * 2] + 0.5 * at(lag as f64 / 2.0);
            (bpm, metre * prior)
        })
        .collect()
}

/// Strongest tempo between 60 and 200 BPM, interpolated between lags.
fn rough_tempo(onsets: &[f32], fps: f64) -> Option<f64> {
    let scores = tempo_scores(onsets, fps);
    let best = (0..scores.len()).max_by(|&a, &b| scores[a].1.total_cmp(&scores[b].1))?;
    if best == 0 || best + 1 == scores.len() {
        return Some(scores[best].0);
    }
    // Parabolic interpolation on the lag axis.
    let lag = |bpm: f64| fps * 60.0 / bpm;
    let (l, c, r) = (scores[best - 1].1, scores[best].1, scores[best + 1].1);
    let denom = l - 2.0 * c + r;
    let offset = if denom.abs() > 1e-12 { 0.5 * (l - r) / denom } else { 0.0 };
    // Scores run from fast (short lag) to slow, one frame of lag apart.
    Some(fps * 60.0 / (lag(scores[best].0) + offset))
}

/// Folds onsets onto one beat period. Returns how peaked the result is
/// (peak over mean) and the peak's phase as a fraction of the period.
fn phase_histogram(onsets: &[f32], period: f64, bins: usize) -> (f32, f64) {
    histogram_peak(&folded(onsets, period, bins))
}

/// Onsets folded onto one beat period, smoothed, relative to their mean.
fn folded(onsets: &[f32], period: f64, bins: usize) -> Vec<f32> {
    let mut hist = vec![0.0f32; bins];
    for (i, &o) in onsets.iter().enumerate() {
        if o > 0.0 {
            let phase = (i as f64 / period).fract();
            hist[((phase * bins as f64) as usize).min(bins - 1)] += o;
        }
    }
    // Circular smoothing: bins are finer than one frame at most tempos.
    let width = ((bins as f64 / period).ceil() as usize).max(1);
    let smoothed: Vec<f32> = (0..bins)
        .map(|b| {
            (0..=2 * width)
                .map(|j| {
                    let w = (width + 1) as f32 - (j as f32 - width as f32).abs();
                    w * hist[(b + bins + j - width) % bins]
                })
                .sum()
        })
        .collect();
    let mean = smoothed.iter().sum::<f32>() / bins as f32;
    if mean <= 0.0 {
        return vec![0.0; bins];
    }
    smoothed.into_iter().map(|v| v / mean).collect()
}

/// Height and phase (fraction of the period) of a folded histogram's peak.
fn histogram_peak(hist: &[f32]) -> (f32, f64) {
    let bins = hist.len();
    let (peak, &value) = hist.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).expect("bins > 0");
    if value <= 0.0 {
        return (0.0, 0.0);
    }
    let (l, r) = (hist[(peak + bins - 1) % bins], hist[(peak + 1) % bins]);
    let denom = l - 2.0 * value + r;
    let offset = if denom.abs() > 1e-9 { 0.5 * (l - r) / denom } else { 0.0 };
    // Bin `b` covers phases [b, b + 1) / bins, so its centre is half a bin in.
    let phase = ((peak as f64 + 0.5 + f64::from(offset)) / bins as f64).rem_euclid(1.0);
    (value, phase)
}

/// Which of the four beats in a bar is the downbeat: the one where the sound
/// changes most from the bar before, since sections and chords change on bars.
fn downbeat_phase(frames: &Frames, first_beat_ms: f64, beat_ms: f64, start_ms: f64) -> u32 {
    let frame_at = |ms: f64| ((ms / 1000.0 * frames.fps).round().max(0.0) as usize).min(frames.spectrum.len());
    let mut beats: Vec<[f32; LOG_BANDS]> = Vec::new();
    let mut t = first_beat_ms;
    while frame_at(t + beat_ms) < frames.spectrum.len() {
        let (lo, hi) = (frame_at(t), frame_at(t + beat_ms).max(frame_at(t) + 1));
        let mut mean = [0.0f32; LOG_BANDS];
        for s in &frames.spectrum[lo..hi] {
            for (m, v) in mean.iter_mut().zip(s) {
                *m += v / (hi - lo) as f32;
            }
        }
        beats.push(mean);
        t += beat_ms;
    }
    if beats.len() < 16 {
        return 0;
    }
    let bar_mean = |k: usize| -> [f32; LOG_BANDS] {
        let mut m = [0.0f32; LOG_BANDS];
        for beat in &beats[k..k + 4] {
            for (a, v) in m.iter_mut().zip(beat) {
                *a += v / 4.0;
            }
        }
        m
    };
    let distance = |a: &[f32; LOG_BANDS], b: &[f32; LOG_BANDS]| a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>();
    let mut scores = [0.0f32; 4];
    for (p, score) in scores.iter_mut().enumerate() {
        let (mut bar, mut beat, mut n) = (0.0, 0.0, 0);
        let mut k = p + 4;
        while k + 4 <= beats.len() {
            bar += distance(&bar_mean(k), &bar_mean(k - 4));
            beat += distance(&beats[k], &beats[k - 1]);
            n += 1;
            k += 4;
        }
        if n > 0 {
            *score = (bar + 0.5 * beat) / n as f32;
        }
    }
    // Songs usually start on a downbeat; use that to break near-ties.
    let first_strong = ((start_ms - first_beat_ms) / beat_ms).round().max(0.0) as usize % 4;
    let average = scores.iter().sum::<f32>() / 4.0;
    scores[first_strong] += 0.05 * average;
    (0..4).max_by(|&a, &b| scores[a].total_cmp(&scores[b])).expect("four phases") as u32
}

/// Temperley's key profiles (Kostka–Payne corpus), tonic first.
const MAJOR: [f32; 12] = [0.748, 0.060, 0.488, 0.082, 0.670, 0.460, 0.096, 0.715, 0.104, 0.366, 0.057, 0.400];
const MINOR: [f32; 12] = [0.712, 0.084, 0.474, 0.618, 0.049, 0.460, 0.105, 0.747, 0.404, 0.067, 0.133, 0.330];
/// Below this correlation the song is treated as having no clear key.
const MIN_KEY_CORRELATION: f32 = 0.35;

fn detect_key(mono: &[f32], rate: u32) -> Option<Key> {
    let chroma = chromagram(mono, rate)?;
    let mut best: Option<(f32, Key)> = None;
    for tonic in 0..12u8 {
        for (minor, profile) in [(false, &MAJOR), (true, &MINOR)] {
            let rotated: Vec<f32> = (0..12).map(|pc| profile[(pc + 12 - usize::from(tonic)) % 12]).collect();
            let r = pearson(&chroma, &rotated);
            if best.is_none_or(|(b, _)| r > b) {
                best = Some((r, Key { tonic, minor }));
            }
        }
    }
    best.filter(|(r, _)| *r >= MIN_KEY_CORRELATION).map(|(_, key)| key)
}

/// Average pitch-class profile over the song, each frame normalised.
fn chromagram(mono: &[f32], rate: u32) -> Option<[f32; 12]> {
    let fft = RealFftPlanner::<f32>::new().plan_fft_forward(KEY_FFT);
    let window = hann(KEY_FFT);
    let mut input = fft.make_input_vec();
    let mut output: Vec<Complex<f32>> = fft.make_output_vec();
    let bin_hz = rate as f32 / KEY_FFT as f32;
    // Map each bin in the pitched range to a pitch class, weighted by how
    // close it lies to the note's centre.
    let bins: Vec<(usize, usize, f32)> = (1..output.len())
        .filter_map(|k| {
            let hz = k as f32 * bin_hz;
            if !(80.0..=5_000.0).contains(&hz) {
                return None;
            }
            let midi = 69.0 + 12.0 * (hz / 440.0).log2();
            let nearest = midi.round();
            let weight = 1.0 - 2.0 * (midi - nearest).abs();
            Some((k, (nearest as i32).rem_euclid(12) as usize, weight))
        })
        .collect();
    let mut total = [0.0f32; 12];
    let mut used = 0;
    let mut start = 0;
    while start + KEY_FFT <= mono.len() {
        for (slot, (x, w)) in input.iter_mut().zip(mono[start..start + KEY_FFT].iter().zip(&window)) {
            *slot = x * w;
        }
        fft.process(&mut input, &mut output).expect("buffer sizes match the plan");
        let mut frame = [0.0f32; 12];
        for &(k, pc, w) in &bins {
            frame[pc] += output[k].norm() * w;
        }
        let sum: f32 = frame.iter().sum();
        if sum > 1e-3 {
            for (t, v) in total.iter_mut().zip(frame) {
                *t += v / sum;
            }
            used += 1;
        }
        start += KEY_FFT / 2;
    }
    (used > 0).then_some(total)
}

fn pearson(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len() as f32;
    let (ma, mb) = (a.iter().sum::<f32>() / n, b.iter().sum::<f32>() / n);
    let cov: f32 = a.iter().zip(b).map(|(x, y)| (x - ma) * (y - mb)).sum();
    let va: f32 = a.iter().map(|x| (x - ma).powi(2)).sum();
    let vb: f32 = b.iter().map(|y| (y - mb).powi(2)).sum();
    if va <= 0.0 || vb <= 0.0 { 0.0 } else { cov / (va * vb).sqrt() }
}

/// Integrated loudness (EBU R128), LUFS.
fn loudness(pcm: &Pcm) -> f32 {
    let Ok(mut meter) = ebur128::EbuR128::new(2, pcm.rate, ebur128::Mode::I) else {
        return -14.0;
    };
    let mut buffer = Vec::with_capacity(1 << 15);
    for chunk in pcm.samples.chunks(1 << 15) {
        buffer.clear();
        buffer.extend(chunk.iter().map(|&s| f32::from(s) / 32768.0));
        if meter.add_frames_f32(&buffer).is_err() {
            return -14.0;
        }
    }
    meter.loudness_global().map(|l| l as f32).ok().filter(|l| l.is_finite()).unwrap_or(-70.0)
}

/// Where the level first and last rises above `SILENCE_DB` of the peak, ms.
fn music_bounds(frames: &Frames) -> (f64, f64) {
    let loudest = frames.rms.iter().fold(0.0f32, |m, &x| m.max(x));
    let threshold = loudest * 10f32.powf(SILENCE_DB / 20.0);
    let first = frames.rms.iter().position(|&r| r > threshold).unwrap_or(0);
    let last = frames.rms.iter().rposition(|&r| r > threshold).map_or(frames.rms.len(), |i| i + 1);
    let ms = |i: usize| i as f64 / frames.fps * 1000.0;
    (ms(first), ms(last).max(ms(first)))
}

/// Loudness, brightness and how busy the onsets are, blended into 0..=1.
fn energy(frames: &Frames, onsets: &[f32], loudness: f32, start_ms: f64, end_ms: f64) -> f32 {
    let range = |ms: f64| ((ms / 1000.0 * frames.fps) as usize).min(frames.rms.len());
    let (lo, hi) = (range(start_ms), range(end_ms));
    if hi <= lo {
        return 0.0;
    }
    let loud = ((loudness + 18.0) / 12.0).clamp(0.0, 1.0);
    let (mut weighted, mut weight) = (0.0f32, 0.0f32);
    for i in lo..hi {
        weighted += frames.centroid[i] * frames.rms[i];
        weight += frames.rms[i];
    }
    let centroid = if weight > 0.0 { weighted / weight } else { 0.0 };
    let bright = ((centroid - 1_000.0) / 2_500.0).clamp(0.0, 1.0);
    let window = &onsets[lo..hi];
    let mean = window.iter().sum::<f32>() / window.len() as f32;
    let std = (window.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / window.len() as f32).sqrt();
    let threshold = mean + std;
    let min_gap = (frames.fps * 0.05) as usize;
    let (mut peaks, mut last) = (0usize, None::<usize>);
    for i in 1..window.len().saturating_sub(1) {
        let is_peak = window[i] > threshold && window[i] >= window[i - 1] && window[i] > window[i + 1];
        if is_peak && last.is_none_or(|l| i - l >= min_gap) {
            peaks += 1;
            last = Some(i);
        }
    }
    let per_second = peaks as f32 / ((hi - lo) as f32 / frames.fps as f32);
    let busy = (per_second / 6.0).clamp(0.0, 1.0);
    (0.5 * loud + 0.25 * bright + 0.25 * busy).clamp(0.0, 1.0)
}

fn waveform(frames: &Frames) -> Vec<u8> {
    let peak_max = frames.peak.iter().fold(1e-6f32, |m, &x| m.max(x));
    let mut band_max = [1e-6f32; 3];
    for b in &frames.bands {
        for (m, v) in band_max.iter_mut().zip(b) {
            *m = m.max(*v);
        }
    }
    let byte = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
    let mut out = Vec::with_capacity(frames.peak.len() * WAVEFORM_STRIDE);
    for (p, b) in frames.peak.iter().zip(&frames.bands) {
        out.push(byte(p / peak_max));
        for (v, m) in b.iter().zip(band_max) {
            out.push(byte((v / m).sqrt()));
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::f32::consts::PI;

    pub const RATE: u32 = 48_000;

    /// Deterministic noise in -1..1.
    pub struct Noise(u32);

    impl Noise {
        pub fn new() -> Noise {
            Noise(0x1234_5678)
        }

        pub fn next(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 17;
            self.0 ^= self.0 << 5;
            self.0 as f32 / u32::MAX as f32 * 2.0 - 1.0
        }
    }

    /// A steady 4/4 track: kick on every beat, hats on off-beats, a chord
    /// that changes every bar from `chords` (each a list of MIDI notes),
    /// starting after `lead_ms` of silence on a downbeat.
    pub fn synth(bpm: f64, seconds: f64, lead_ms: f64, chords: &[&[i32]]) -> Pcm {
        let frames = (seconds * f64::from(RATE)) as usize;
        let beat = 60.0 / bpm;
        let lead = lead_ms / 1000.0;
        let mut noise = Noise::new();
        let mut out = vec![0.0f32; frames * 2];
        for i in 0..frames {
            let t = i as f64 / f64::from(RATE);
            if t < lead {
                continue;
            }
            let since = t - lead;
            let beat_index = (since / beat).floor();
            let in_beat = since - beat_index * beat;
            let bar = (beat_index / 4.0).floor() as usize;
            // Kick: pitch drops from 150 to 50 Hz, decays fast.
            let kick_env = (-in_beat * 30.0).exp();
            let kick = (2.0 * std::f64::consts::PI * (50.0 * in_beat + 100.0 / 30.0 * (1.0 - (-in_beat * 30.0).exp()))).sin() * kick_env;
            // Off-beat hat.
            let off = in_beat - beat / 2.0;
            let hat = if off >= 0.0 { f64::from(noise.next()) * (-off * 80.0).exp() * 0.3 } else { 0.0 };
            // Chord pad, re-attacked at each bar.
            let chord = chords[bar % chords.len()];
            let since_bar = since - (bar as f64) * 4.0 * beat;
            let pad_env = 0.25 * (1.0 - (-since_bar * 20.0).exp());
            let pad: f64 = chord
                .iter()
                .map(|&n| {
                    let hz = 440.0 * 2f64.powf(f64::from(n - 69) / 12.0);
                    (2.0 * std::f64::consts::PI * hz * t).sin() + 0.3 * (4.0 * std::f64::consts::PI * hz * t).sin()
                })
                .sum::<f64>()
                * pad_env
                / chord.len() as f64;
            let s = (0.6 * kick + hat + pad) as f32 * 0.7;
            out[i * 2] = s;
            out[i * 2 + 1] = s;
        }
        Pcm::from_f32(RATE, &out)
    }

    // A minor: Am, Dm, Em, Am.
    pub const A_MINOR: [&[i32]; 4] = [&[57, 60, 64], &[62, 65, 69], &[64, 67, 71], &[57, 60, 64]];
    // C major: C, F, G, C.
    pub const C_MAJOR: [&[i32]; 4] = [&[60, 64, 67], &[65, 69, 72], &[67, 71, 74], &[60, 64, 67]];

    #[test]
    fn finds_tempo_and_downbeat() {
        for (bpm, lead) in [(128.0, 300.0), (124.0, 1_000.0), (95.0, 120.0), (140.0, 50.0), (126.4, 200.0), (87.0, 0.0)] {
            let a = analyze(&synth(bpm, 60.0, lead, &A_MINOR)).analysis;
            let grid = a.grid.unwrap_or_else(|| panic!("{bpm}: no grid"));
            assert!((grid.bpm - bpm).abs() < 0.03, "{bpm}: found {}", grid.bpm);
            // The first downbeat is at `lead`, maybe a whole bar earlier in the silence.
            let bar = grid.bar_ms();
            let off = (grid.downbeat_ms - lead).rem_euclid(bar);
            let off = off.min(bar - off);
            assert!(off < 8.0, "{bpm}: downbeat {} vs {lead}", grid.downbeat_ms);
            eprintln!("{bpm}: bpm {:.3} downbeat {:.1} (lead {lead}, off {off:.1})", grid.bpm, grid.downbeat_ms);
        }
    }

    #[test]
    fn finds_keys() {
        let a = analyze(&synth(120.0, 30.0, 0.0, &A_MINOR)).analysis;
        assert_eq!(a.key, Some(Key { tonic: 9, minor: true }));
        let c = analyze(&synth(120.0, 30.0, 0.0, &C_MAJOR)).analysis;
        assert_eq!(c.key, Some(Key { tonic: 0, minor: false }));
    }

    #[test]
    fn short_clips_have_no_grid() {
        assert_eq!(analyze(&synth(128.0, 10.0, 0.0, &A_MINOR)).analysis.grid, None);
    }

    #[test]
    fn noise_has_no_grid() {
        let mut noise = Noise::new();
        let samples: Vec<f32> = (0..RATE as usize * 30 * 2).map(|_| noise.next() * 0.3).collect();
        let a = analyze(&Pcm::from_f32(RATE, &samples)).analysis;
        assert_eq!(a.grid, None);
    }

    #[test]
    fn finds_where_the_music_is_and_how_loud() {
        let pcm = synth(120.0, 30.0, 2_000.0, &A_MINOR);
        let mut samples: Vec<f32> = pcm.samples.iter().map(|&s| f32::from(s) / 32768.0).collect();
        samples.extend(std::iter::repeat_n(0.0, RATE as usize * 3 * 2));
        let a = analyze(&Pcm::from_f32(RATE, &samples)).analysis;
        assert!((a.start_ms - 2_000.0).abs() < 30.0, "start {}", a.start_ms);
        assert!((a.end_ms - 30_000.0).abs() < 300.0, "end {}", a.end_ms);
        assert!(a.loudness > -30.0 && a.loudness < -3.0, "{}", a.loudness);
        assert!((0.0..=1.0).contains(&a.energy));
    }

    #[test]
    fn quiet_sparse_songs_have_less_energy() {
        let loud = analyze(&synth(128.0, 30.0, 0.0, &A_MINOR)).analysis;
        // Same pad, far quieter and with no drums.
        let samples: Vec<f32> = (0..RATE as usize * 30)
            .flat_map(|i| {
                let t = i as f32 / RATE as f32;
                let s = 0.05 * (2.0 * PI * 220.0 * t).sin();
                [s, s]
            })
            .collect();
        let calm = analyze(&Pcm::from_f32(RATE, &samples)).analysis;
        assert!(loud.energy > calm.energy + 0.15, "{} vs {}", loud.energy, calm.energy);
    }

    #[test]
    fn waveform_has_a_column_per_hundredth() {
        let out = analyze(&synth(120.0, 10.0, 0.0, &A_MINOR));
        assert_eq!(out.waveform.len() / WAVEFORM_STRIDE, 1_000);
        assert!(out.waveform.chunks(4).any(|c| c[0] == 255));
    }
}
