//! How well one track flows into another.

use crate::camelot::{Camelot, KeyRelation};
use crate::model::Analysis;
use serde::{Deserialize, Serialize};

const KEY_WEIGHT: f32 = 0.4;
const TEMPO_WEIGHT: f32 = 0.4;
const ENERGY_WEIGHT: f32 = 0.2;
/// Tempo difference (as a fraction) at which the tempo score falls to 1/e.
const TEMPO_TOLERANCE: f32 = 0.06;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Compat {
    /// 0..=1, higher flows better.
    pub score: f32,
    pub key: Option<KeyMatch>,
    pub tempo: Option<TempoMatch>,
    pub energy: Option<EnergyMatch>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct KeyMatch {
    pub from: Camelot,
    pub to: Camelot,
    pub relation: KeyRelation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TempoRelation {
    Direct,
    /// The next track is at roughly double the tempo.
    Double,
    /// The next track is at roughly half the tempo.
    Half,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TempoMatch {
    pub from: f64,
    pub to: f64,
    pub relation: TempoRelation,
    /// Signed difference after half/double-time folding, as a fraction of `from`.
    pub diff: f64,
    pub score: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EnergyMatch {
    pub from: f32,
    pub to: f32,
    pub score: f32,
}

impl EnergyMatch {
    pub fn delta(&self) -> f32 {
        self.to - self.from
    }
}

/// What an unknown key, tempo or energy scores: dejay can't vouch for it, so
/// songs analysis couldn't read rank below ones it could.
const UNKNOWN: f32 = 0.25;

pub fn compat(from: Option<&Analysis>, to: Option<&Analysis>) -> Compat {
    let (Some(a), Some(b)) = (from, to) else {
        return Compat { score: UNKNOWN, key: None, tempo: None, energy: None };
    };
    let key = key_match(a, b);
    let tempo = tempo_match(a, b);
    let energy = Some(energy_match(a, b));

    let score = KEY_WEIGHT * key.map_or(UNKNOWN, |k| k.relation.score())
        + TEMPO_WEIGHT * tempo.map_or(UNKNOWN, |t| t.score)
        + ENERGY_WEIGHT * energy.map_or(UNKNOWN, |e| e.score);
    Compat { score, key, tempo, energy }
}

fn key_match(a: &Analysis, b: &Analysis) -> Option<KeyMatch> {
    let from: Camelot = a.camelot()?;
    let to = b.camelot()?;
    Some(KeyMatch { from, to, relation: KeyRelation::between(from, to) })
}

fn tempo_match(a: &Analysis, b: &Analysis) -> Option<TempoMatch> {
    let (from, to) = (a.bpm()?, b.bpm()?);
    let fold = |relation, factor: f64| (relation, (to * factor - from) / from);
    let (relation, diff) = [
        fold(TempoRelation::Direct, 1.0),
        fold(TempoRelation::Double, 0.5),
        fold(TempoRelation::Half, 2.0),
    ]
    .into_iter()
    .min_by(|x, y| x.1.abs().total_cmp(&y.1.abs()))
    .expect("three candidates");
    // Half/double time is mixable but not as smooth as a straight match.
    let penalty = if relation == TempoRelation::Direct { 1.0 } else { 0.85 };
    let score = (-(diff as f32 / TEMPO_TOLERANCE).powi(2)).exp() * penalty;
    Some(TempoMatch { from, to, relation, diff, score })
}

fn energy_match(a: &Analysis, b: &Analysis) -> EnergyMatch {
    let delta = b.energy - a.energy;
    // Holding steady or building gently is ideal; big swings either way hurt.
    let score = if (0.0..=0.15).contains(&delta) {
        1.0
    } else {
        (1.0 - (delta.abs() - 0.05).max(0.0) / 0.45).clamp(0.0, 1.0)
    };
    EnergyMatch { from: a.energy, to: b.energy, score }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    use crate::model::{Grid, Key};

    /// Analysis with the given tempo (0 = none), key (tonic, or -1 for none; mode 1 = major) and energy.
    pub fn feat(tempo: f64, key: i8, mode: u8, energy: f32) -> Analysis {
        Analysis {
            version: 1,
            grid: (tempo > 0.0).then_some(Grid { bpm: tempo, downbeat_ms: 0.0 }),
            key: (key >= 0).then_some(Key { tonic: key as u8, minor: mode == 0 }),
            energy,
            loudness: -8.0,
            start_ms: 0.0,
            end_ms: 200_000.0,
        }
    }

    #[test]
    fn identical_tracks_score_perfectly() {
        let a = feat(124.0, 9, 0, 0.7);
        let c = compat(Some(&a), Some(&a));
        assert!((c.score - 1.0).abs() < 1e-6, "{c:?}");
    }

    #[test]
    fn folds_half_and_double_time() {
        let a = feat(87.0, 0, 1, 0.5);
        let b = feat(174.0, 0, 1, 0.5);
        let t = compat(Some(&a), Some(&b)).tempo.unwrap();
        assert_eq!(t.relation, TempoRelation::Double);
        assert!(t.diff.abs() < 1e-6);
    }

    #[test]
    fn harmonic_neighbour_beats_a_clash() {
        let a = feat(124.0, 9, 0, 0.7); // 8A
        let neighbour = feat(124.0, 4, 0, 0.7); // 9A
        let clash = feat(124.0, 3, 0, 0.7); // 2A
        let good = compat(Some(&a), Some(&neighbour)).score;
        let bad = compat(Some(&a), Some(&clash)).score;
        assert!(good > bad + 0.2, "{good} vs {bad}");
    }

    #[test]
    fn tempo_score_drops_with_distance() {
        let a = feat(120.0, 0, 1, 0.5);
        let near = compat(Some(&a), Some(&feat(122.0, 0, 1, 0.5))).score;
        let far = compat(Some(&a), Some(&feat(140.0, 0, 1, 0.5))).score;
        assert!(near > far);
    }

    #[test]
    fn missing_analysis_scores_low() {
        let a = feat(120.0, 0, 1, 0.5);
        assert_eq!(compat(Some(&a), None).score, UNKNOWN);
        assert_eq!(compat(None, None).score, UNKNOWN);
    }

    #[test]
    fn unknown_key_counts_as_a_poor_match() {
        let a = feat(120.0, -1, 1, 0.5);
        let c = compat(Some(&a), Some(&a));
        assert!(c.key.is_none());
        assert!((c.score - 0.7).abs() < 1e-6, "{}", c.score);
        // Nothing known but energy can't beat a real match.
        let blank = feat(0.0, -1, 1, 0.5);
        let known = feat(120.0, 9, 0, 0.5);
        assert!(compat(Some(&known), Some(&blank)).score < compat(Some(&known), Some(&known)).score);
    }
}
