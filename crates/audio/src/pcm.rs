//! Decoded audio held in memory, and decoding through ffmpeg.

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

/// Interleaved stereo, 16 bits per sample to halve the memory a decoded song takes.
pub struct Pcm {
    pub rate: u32,
    pub samples: Vec<i16>,
}

const SCALE: f32 = 1.0 / 32768.0;

impl Pcm {
    pub fn from_f32(rate: u32, interleaved: &[f32]) -> Pcm {
        let samples = interleaved.iter().map(|&s| (s.clamp(-1.0, 1.0) * 32767.0).round() as i16).collect();
        Pcm { rate, samples }
    }

    pub fn frames(&self) -> usize {
        self.samples.len() / 2
    }

    pub fn duration_ms(&self) -> f64 {
        self.frames() as f64 * 1000.0 / f64::from(self.rate)
    }

    /// Silence outside the song.
    #[inline]
    pub fn frame(&self, index: i64) -> [f32; 2] {
        if index < 0 || index as usize >= self.frames() {
            return [0.0; 2];
        }
        let i = index as usize * 2;
        [f32::from(self.samples[i]) * SCALE, f32::from(self.samples[i + 1]) * SCALE]
    }

    /// Linear interpolation between frames.
    #[inline]
    pub fn frame_at(&self, position: f64) -> [f32; 2] {
        let base = position.floor();
        let t = (position - base) as f32;
        let (a, b) = (self.frame(base as i64), self.frame(base as i64 + 1));
        [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
    }

    pub fn mono(&self) -> Vec<f32> {
        self.samples.as_chunks::<2>().0.iter().map(|f| (f32::from(f[0]) + f32::from(f[1])) * 0.5 * SCALE).collect()
    }
}

/// Decodes anything ffmpeg reads into stereo at `rate`.
pub fn decode(ffmpeg: &Path, path: &Path, rate: u32) -> Result<Pcm> {
    let mut child = Command::new(ffmpeg)
        .args(["-nostdin", "-v", "error", "-i"])
        .arg(path)
        .args(["-vn", "-f", "s16le", "-acodec", "pcm_s16le", "-ac", "2", "-ar"])
        .arg(rate.to_string())
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {}", ffmpeg.display()))?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut samples = Vec::new();
    let mut buffer = vec![0u8; 1 << 16];
    let mut carry: Option<u8> = None;
    loop {
        let n = stdout.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        let mut bytes = &buffer[..n];
        if let Some(low) = carry.take() {
            samples.push(i16::from_le_bytes([low, bytes[0]]));
            bytes = &bytes[1..];
        }
        let (pairs, rest) = bytes.as_chunks::<2>();
        if let [odd] = rest {
            carry = Some(*odd);
        }
        samples.extend(pairs.iter().map(|b| i16::from_le_bytes(*b)));
    }
    let mut stderr = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut stderr);
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("ffmpeg could not decode {}: {}", path.display(), stderr.trim().lines().last().unwrap_or("unknown error"));
    }
    if samples.len() % 2 == 1 {
        samples.pop();
    }
    if samples.is_empty() {
        bail!("{} contains no audio", path.display());
    }
    Ok(Pcm { rate, samples })
}
