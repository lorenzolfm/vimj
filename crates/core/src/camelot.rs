//! Camelot wheel notation for harmonic mixing.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Camelot {
    /// 1..=12 around the wheel.
    pub number: u8,
    /// `A` on the wheel; major keys are `B`.
    pub minor: bool,
}

impl Camelot {
    /// Places a key, given by its tonic's pitch class (0 = C), on the wheel.
    pub fn from_key(tonic: u8, minor: bool) -> Camelot {
        let tonic = tonic % 12;
        // A minor key sits on the same number as its relative major,
        // three semitones up.
        let major_root = if minor { (tonic + 3) % 12 } else { tonic };
        // Each step around the wheel is a fifth (7 semitones); C major is 8B.
        let number = (major_root * 7 + 7) % 12 + 1;
        Camelot { number, minor }
    }

    /// Steps clockwise from `self` to `other`, 0..12.
    fn steps_to(self, other: Camelot) -> u8 {
        (other.number + 12 - self.number) % 12
    }
}

impl fmt::Display for Camelot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}", self.number, if self.minor { 'A' } else { 'B' })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyRelation {
    Same,
    /// Relative major/minor: same number, other letter.
    Relative,
    /// One step around the wheel, same letter.
    Adjacent,
    /// Two steps clockwise: a classic energy boost.
    Boost,
    /// One step around the wheel and switching letter.
    Diagonal,
    /// Seven steps clockwise: up one semitone.
    Semitone,
    Clash,
}

impl KeyRelation {
    pub fn between(a: Camelot, b: Camelot) -> KeyRelation {
        let steps = a.steps_to(b);
        let same_letter = a.minor == b.minor;
        match (steps, same_letter) {
            (0, true) => KeyRelation::Same,
            (0, false) => KeyRelation::Relative,
            (1 | 11, true) => KeyRelation::Adjacent,
            (1 | 11, false) => KeyRelation::Diagonal,
            (2, true) => KeyRelation::Boost,
            (7, true) => KeyRelation::Semitone,
            _ => KeyRelation::Clash,
        }
    }

    pub fn score(self) -> f32 {
        match self {
            KeyRelation::Same => 1.0,
            KeyRelation::Relative | KeyRelation::Adjacent => 0.9,
            KeyRelation::Boost => 0.65,
            KeyRelation::Diagonal => 0.55,
            KeyRelation::Semitone => 0.5,
            KeyRelation::Clash => 0.15,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            KeyRelation::Same => "same key",
            KeyRelation::Relative => "relative key",
            KeyRelation::Adjacent => "adjacent key",
            KeyRelation::Boost => "key boost",
            KeyRelation::Diagonal => "diagonal key",
            KeyRelation::Semitone => "semitone up",
            KeyRelation::Clash => "key clash",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(tonic: u8, mode: u8) -> String {
        Camelot::from_key(tonic, mode == 0).to_string()
    }

    #[test]
    fn maps_keys_onto_the_wheel() {
        // Majors.
        assert_eq!(cam(0, 1), "8B"); // C
        assert_eq!(cam(7, 1), "9B"); // G
        assert_eq!(cam(5, 1), "7B"); // F
        assert_eq!(cam(11, 1), "1B"); // B
        assert_eq!(cam(6, 1), "2B"); // F#
        assert_eq!(cam(1, 1), "3B"); // Db
        // Minors.
        assert_eq!(cam(9, 0), "8A"); // Am
        assert_eq!(cam(0, 0), "5A"); // Cm
        assert_eq!(cam(4, 0), "9A"); // Em
        assert_eq!(cam(8, 0), "1A"); // G#m
        assert_eq!(cam(1, 0), "12A"); // C#m
    }

    #[test]
    fn classifies_relations() {
        let at = |number, minor| Camelot { number, minor };
        let rel = KeyRelation::between;
        assert_eq!(rel(at(8, true), at(8, true)), KeyRelation::Same);
        assert_eq!(rel(at(8, true), at(8, false)), KeyRelation::Relative);
        assert_eq!(rel(at(8, true), at(9, true)), KeyRelation::Adjacent);
        assert_eq!(rel(at(1, true), at(12, true)), KeyRelation::Adjacent);
        assert_eq!(rel(at(8, true), at(9, false)), KeyRelation::Diagonal);
        assert_eq!(rel(at(8, true), at(10, true)), KeyRelation::Boost);
        assert_eq!(rel(at(8, false), at(3, false)), KeyRelation::Semitone);
        assert_eq!(rel(at(8, true), at(2, true)), KeyRelation::Clash);
    }
}
