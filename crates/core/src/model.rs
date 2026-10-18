use crate::camelot::Camelot;
use serde::{Deserialize, Serialize};

/// A song in the local library.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Song {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub duration_ms: u64,
    /// File name inside the music directory.
    pub file: String,
    /// The page it was downloaded from.
    #[serde(default)]
    pub source: Option<String>,
    /// Unix seconds.
    #[serde(default)]
    pub added: u64,
    #[serde(default)]
    pub analysis: Option<Analysis>,
}

/// What analysis found in a song's audio.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Analysis {
    /// Analysis code version, so that old results can be redone.
    pub version: u32,
    pub grid: Option<Grid>,
    pub key: Option<Key>,
    /// 0..=1, how intense the song feels.
    pub energy: f32,
    /// Integrated loudness, LUFS.
    pub loudness: f32,
    /// Where the music starts and ends, past leading and trailing silence.
    pub start_ms: f64,
    pub end_ms: f64,
}

impl Analysis {
    pub fn bpm(&self) -> Option<f64> {
        self.grid.map(|g| g.bpm)
    }

    pub fn camelot(&self) -> Option<Camelot> {
        self.key.map(Key::camelot)
    }
}

/// A constant-tempo beat grid in 4/4.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Grid {
    pub bpm: f64,
    /// The earliest downbeat (first beat of a bar), at or after 0.
    pub downbeat_ms: f64,
}

pub const BEATS_PER_BAR: u32 = 4;

impl Grid {
    pub fn beat_ms(&self) -> f64 {
        60_000.0 / self.bpm
    }

    pub fn bar_ms(&self) -> f64 {
        self.beat_ms() * f64::from(BEATS_PER_BAR)
    }

    /// Beats since the first downbeat; fractional between beats.
    pub fn beat_at(&self, ms: f64) -> f64 {
        (ms - self.downbeat_ms) / self.beat_ms()
    }

    pub fn beat_time(&self, beat: f64) -> f64 {
        self.downbeat_ms + beat * self.beat_ms()
    }

    pub fn snap_to_beat(&self, ms: f64) -> f64 {
        self.beat_time(self.beat_at(ms).round())
    }

    pub fn snap_to_bar(&self, ms: f64) -> f64 {
        let bars = self.beat_at(ms) / f64::from(BEATS_PER_BAR);
        self.beat_time(bars.round() * f64::from(BEATS_PER_BAR))
    }

    /// Whether `ms` sits on a downbeat (within 1 ms).
    pub fn is_downbeat(&self, ms: f64) -> bool {
        let beat = self.beat_at(ms);
        (beat - beat.round()).abs() * self.beat_ms() < 1.0 && beat.round().rem_euclid(4.0) == 0.0
    }
}

/// A musical key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Key {
    /// Pitch class of the tonic: 0 = C … 11 = B.
    pub tonic: u8,
    pub minor: bool,
}

const NOTE_NAMES: [&str; 12] = ["C", "C♯", "D", "E♭", "E", "F", "F♯", "G", "A♭", "A", "B♭", "B"];

impl Key {
    pub fn camelot(self) -> Camelot {
        Camelot::from_key(self.tonic, self.minor)
    }

    /// `Am`, `F♯`.
    pub fn name(self) -> String {
        format!("{}{}", NOTE_NAMES[usize::from(self.tonic % 12)], if self.minor { "m" } else { "" })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_maps_between_time_and_beats() {
        let g = Grid { bpm: 120.0, downbeat_ms: 250.0 };
        assert_eq!(g.beat_ms(), 500.0);
        assert_eq!(g.beat_at(1250.0), 2.0);
        assert_eq!(g.beat_time(-0.5), 0.0);
        assert_eq!(g.snap_to_beat(1_390.0), 1_250.0);
        assert_eq!(g.snap_to_bar(1_390.0), 2_250.0);
        assert!(g.is_downbeat(2_250.0));
        assert!(!g.is_downbeat(1_250.0));
    }

    #[test]
    fn names_keys() {
        assert_eq!(Key { tonic: 9, minor: true }.name(), "Am");
        assert_eq!(Key { tonic: 6, minor: false }.name(), "F♯");
        assert_eq!(Key { tonic: 9, minor: true }.camelot().to_string(), "8A");
    }
}
