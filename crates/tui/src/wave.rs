//! Reading the waveforms the daemon serves: four bytes per column (peak,
//! low, mid, high) at `WAVEFORM_HZ`.

use crate::theme;
use vimj_core::protocol::WAVEFORM_HZ;
use ratatui::style::Color;

const STRIDE: usize = 4;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Column {
    /// 0..=1.
    pub peak: f32,
    /// Low, mid, high, 0..=1.
    pub bands: [f32; 3],
}

/// The loudest moment in `[from_ms, to_ms)`, or `None` outside the song.
pub fn column(wave: &[u8], from_ms: f64, to_ms: f64) -> Option<Column> {
    let count = wave.len() / STRIDE;
    let index = |ms: f64| (ms / 1000.0 * WAVEFORM_HZ).floor();
    let (lo, hi) = (index(from_ms), index(to_ms).max(index(from_ms) + 1.0));
    if hi <= 0.0 || lo >= count as f64 {
        return None;
    }
    let (lo, hi) = (lo.max(0.0) as usize, (hi as usize).min(count));
    let mut col = Column::default();
    for chunk in wave[lo * STRIDE..hi * STRIDE].as_chunks::<STRIDE>().0 {
        col.peak = col.peak.max(f32::from(chunk[0]) / 255.0);
        for (b, &v) in col.bands.iter_mut().zip(&chunk[1..]) {
            *b = b.max(f32::from(v) / 255.0);
        }
    }
    Some(col)
}

/// Bass-heavy columns lean blue, bright ones lean towards white, like a DJ waveform.
pub fn color(col: Column, lit: bool) -> Color {
    let [low, mid, high] = col.bands;
    let total = (low + mid + high).max(1e-3);
    let hue = 210.0 - 170.0 * ((mid + high * 1.5) / (total * 1.5)).clamp(0.0, 1.0);
    if lit { theme::hsl(hue, 0.75, 0.62) } else { theme::hsl(hue, 0.25, 0.32) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_loudest_column_in_a_span() {
        // 3 columns at 100 per second: 0..10, 10..20, 20..30 ms.
        let wave = [10, 0, 0, 0, 255, 255, 0, 0, 20, 0, 0, 255];
        let c = column(&wave, 0.0, 30.0).unwrap();
        assert_eq!(c.peak, 1.0);
        assert_eq!(c.bands, [1.0, 0.0, 1.0]);
        assert_eq!(column(&wave, 20.0, 21.0).unwrap().peak, 20.0 / 255.0);
        assert_eq!(column(&wave, 40.0, 50.0), None);
        assert_eq!(column(&wave, -50.0, -10.0), None);
    }
}
