//! Automation curves, as Spotify's curve editor draws them: points joined by
//! segments that either ease in and out or run straight, each of which can be
//! bent up or down. A segment is a cubic Bézier in value over linear time.

use serde::{Deserialize, Serialize};

/// How far a bend of ±1 moves a segment's control points.
const BEND_REACH: f32 = 2.0 / 3.0;
/// Closest two points may come in time.
pub const MIN_GAP: f32 = 0.001;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Progress through the transition, 0..=1.
    pub x: f32,
    /// 0..=1.
    pub y: f32,
    /// The segment to the next point runs straight instead of easing in and out.
    #[serde(default, skip_serializing_if = "is_false")]
    pub linear: bool,
    /// Bows the segment to the next point up (above 0) or down, as far as
    /// keeps it within 0..=1.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bend: f32,
}

fn is_false(b: &bool) -> bool {
    !b
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

/// An eased point.
pub const fn p(x: f32, y: f32) -> Point {
    Point { x, y, linear: false, bend: 0.0 }
}

/// A point whose segment to the next runs straight, bent by `bend`.
pub const fn line(x: f32, y: f32, bend: f32) -> Point {
    Point { x, y, linear: true, bend }
}

/// The value at `x`. Before the first point and after the last, the curve holds.
pub fn value(points: &[Point], x: f32) -> f32 {
    let Some(first) = points.first() else { return 0.0 };
    if x <= first.x {
        return first.y;
    }
    for pair in points.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if x < b.x {
            let t = (x - a.x) / (b.x - a.x);
            let (c1, c2) = controls(a, b);
            let u = 1.0 - t;
            // Relative to the start, so flat segments come out exact.
            return a.y + 3.0 * u * u * t * (c1 - a.y) + 3.0 * u * t * t * (c2 - a.y) + t * t * t * (b.y - a.y);
        }
    }
    points[points.len() - 1].y
}

/// Heights of the two control points of the segment from `a` to `b`.
fn controls(a: Point, b: Point) -> (f32, f32) {
    let (c1, c2) = base_controls(a, b);
    let (low, high) = bend_range(c1, c2);
    let bend = a.bend.clamp(low, high) * BEND_REACH;
    (c1 + bend, c2 + bend)
}

fn base_controls(a: Point, b: Point) -> (f32, f32) {
    let rise = b.y - a.y;
    if a.linear { (a.y + rise / 3.0, a.y + 2.0 * rise / 3.0) } else { (a.y, b.y) }
}

/// Bends that keep both control points within 0..=1.
fn bend_range(c1: f32, c2: f32) -> (f32, f32) {
    let low = (-c1.min(c2) / BEND_REACH).max(-1.0);
    let high = ((1.0 - c1.max(c2)) / BEND_REACH).min(1.0);
    (low.min(0.0), high.max(0.0))
}

/// A curve across the whole window: points from 0 to 1, each to the right
/// of the one before, with values within 0..=1. Two ends make at least two
/// points. Stored and sent curves are parsed into this, and the edits below
/// keep it so.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "Vec<Point>", into = "Vec<Point>")]
pub struct Curve(Vec<Point>);

impl TryFrom<Vec<Point>> for Curve {
    type Error = String;

    fn try_from(points: Vec<Point>) -> Result<Curve, String> {
        if points.first().zip(points.last()).map(|(a, b)| (a.x, b.x)) != Some((0.0, 1.0)) {
            return Err("a curve runs from 0 to 1 across the window".into());
        }
        // Not MIN_GAP apart: points moved up against a neighbour land a
        // rounding error closer than that.
        if !points.windows(2).all(|w| w[0].x < w[1].x) {
            return Err("a curve's points go from left to right".into());
        }
        if !points.iter().all(|q| (0.0..=1.0).contains(&q.y) && q.bend.is_finite()) {
            return Err("a curve's values are within 0 and 1".into());
        }
        Ok(Curve(points))
    }
}

impl From<Curve> for Vec<Point> {
    fn from(curve: Curve) -> Vec<Point> {
        curve.0
    }
}

impl std::ops::Deref for Curve {
    type Target = [Point];

    fn deref(&self) -> &[Point] {
        &self.0
    }
}

impl Curve {
    /// Moves point `i` to (`x`, `y`). The first and last points only move up
    /// and down; the others stay between their neighbours.
    pub fn move_point(&mut self, i: usize, x: f32, y: f32) {
        if x.is_nan() || y.is_nan() {
            return;
        }
        let points = &mut self.0;
        if i != 0 && i + 1 != points.len() {
            let (low, high) = (points[i - 1].x + MIN_GAP, points[i + 1].x - MIN_GAP);
            // Neighbours closer than that leave the point no room to move.
            if low <= high {
                points[i].x = x.clamp(low, high);
            }
        }
        points[i].y = y.clamp(0.0, 1.0);
    }

    /// Adds a point on the curve at `x`, splitting the segment there into two
    /// of the same kind, and returns where it went. `None` when `x` is outside
    /// the curve or too close to a point already there.
    pub fn insert(&mut self, x: f32) -> Option<usize> {
        let points = &mut self.0;
        let at = points.iter().position(|q| q.x > x)?;
        if at == 0 || x - points[at - 1].x < MIN_GAP || points[at].x - x < MIN_GAP {
            return None;
        }
        // Rounding can carry an eased segment a hair past 0 or 1.
        let y = value(points, x).clamp(0.0, 1.0);
        let linear = points[at - 1].linear;
        points[at - 1].bend = 0.0;
        points.insert(at, Point { x, y, linear, bend: 0.0 });
        Some(at)
    }

    /// Takes point `i` out, unless it is the first or the last.
    pub fn remove(&mut self, i: usize) -> bool {
        if i == 0 || i + 1 >= self.0.len() {
            return false;
        }
        self.0.remove(i);
        true
    }

    /// Switches the segment after point `i` between eased and straight.
    pub fn toggle_linear(&mut self, i: usize) {
        if i + 1 < self.0.len() {
            self.0[i].linear = !self.0[i].linear;
            self.0[i].bend = 0.0;
        }
    }

    /// Bends the segment after point `i` by `by`, within what keeps it in 0..=1.
    pub fn bend(&mut self, i: usize, by: f32) {
        if i + 1 >= self.0.len() || by.is_nan() {
            return;
        }
        let (c1, c2) = base_controls(self.0[i], self.0[i + 1]);
        let (low, high) = bend_range(c1, c2);
        self.0[i].bend = (self.0[i].bend + by).clamp(low, high);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    fn curve(points: &[Point]) -> Curve {
        Curve::try_from(points.to_vec()).unwrap()
    }

    #[test]
    fn only_points_across_the_whole_window_make_a_curve() {
        let parse = |json: &str| serde_json::from_str::<Curve>(json).map(Vec::from);
        let good = r#"[{"x":0,"y":1},{"x":0.5,"y":0.25,"linear":true},{"x":1,"y":0}]"#;
        assert_eq!(parse(good).unwrap().len(), 3);
        for (bad, why) in [
            ("[]", "no points"),
            (r#"[{"x":0,"y":1}]"#, "one point"),
            (r#"[{"x":0.2,"y":1},{"x":1,"y":0}]"#, "starts after the window does"),
            (r#"[{"x":0,"y":1},{"x":0.7,"y":0}]"#, "ends before it does"),
            (r#"[{"x":0,"y":1},{"x":0.6,"y":1},{"x":0.4,"y":0},{"x":1,"y":0}]"#, "goes back"),
            (r#"[{"x":0,"y":1},{"x":0.5,"y":1},{"x":0.5,"y":0},{"x":1,"y":0}]"#, "two points at once"),
            (r#"[{"x":0,"y":5},{"x":1,"y":0}]"#, "above 1"),
            (r#"[{"x":0,"y":1},{"x":1,"y":-1}]"#, "below 0"),
        ] {
            assert!(parse(bad).is_err(), "{why}");
        }
    }

    #[test]
    fn edits_keep_a_curve_a_curve() {
        // Any run of edits, from any start, must still parse: else the
        // daemon would refuse to store what the editor drew.
        let mut seed = 1u64;
        let mut next = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            (seed >> 40) as f32 / (1u64 << 24) as f32
        };
        for _ in 0..2_000 {
            let mut c = curve(&[p(0.0, next()), line(1.0, next(), 0.0)]);
            for _ in 0..16 {
                let i = ((next() * c.len() as f32) as usize).min(c.len() - 1);
                match (next() * 5.0) as usize {
                    0 => _ = c.insert(next()),
                    1 => c.move_point(i, c[i].x + (next() - 0.5) * 0.4, c[i].y + next() - 0.5),
                    2 => c.bend(i, (next() - 0.5) * 4.0),
                    3 => c.toggle_linear(i),
                    _ => _ = c.remove(i),
                }
            }
            assert!(Curve::try_from(c.to_vec()).is_ok(), "{c:?}");
        }
    }

    #[test]
    fn points_crowded_by_their_neighbours_stay_put() {
        // Closer than two MIN_GAPs, as a curve from elsewhere may be.
        let mut c = curve(&[p(0.0, 0.0), p(0.5, 0.2), p(0.5005, 0.4), p(0.501, 0.6), p(1.0, 1.0)]);
        c.move_point(2, 0.9, 0.5);
        assert_eq!((c[2].x, c[2].y), (0.5005, 0.5));
    }

    #[test]
    fn eased_segments_are_smoothsteps_and_straight_ones_lines() {
        let eased = [p(0.0, 0.0), p(1.0, 1.0)];
        for x in [0.1f32, 0.25, 0.5, 0.8] {
            assert!(close(value(&eased, x), x * x * (3.0 - 2.0 * x)), "{x}");
            assert!(close(value(&[line(0.0, 0.0, 0.0), p(1.0, 1.0)], x), x), "{x}");
        }
    }

    #[test]
    fn curves_hold_past_their_ends() {
        let c = [p(0.2, 0.3), p(0.6, 0.9)];
        assert_eq!(value(&c, 0.0), 0.3);
        assert_eq!(value(&c, 1.0), 0.9);
        assert_eq!(value(&[], 0.5), 0.0);
    }

    #[test]
    fn bends_bow_segments_but_stay_in_range() {
        let mut c = curve(&[line(0.0, 0.0, 0.0), p(1.0, 1.0)]);
        c.bend(0, 5.0);
        assert!(close(c[0].bend, 0.5));
        assert!(close(value(&c, 0.5), 0.75));
        assert!((0..=20).all(|i| (0.0..=1.0).contains(&value(&c, i as f32 / 20.0))));
        c.bend(0, -10.0);
        assert!(close(value(&c, 0.5), 0.25));
        // An eased segment from 1 to 0 already touches both limits.
        let mut d = curve(&[p(0.0, 1.0), p(1.0, 0.0)]);
        d.bend(0, 1.0);
        d.bend(0, -1.0);
        assert_eq!(d[0].bend, 0.0);
    }

    #[test]
    fn inner_points_stay_between_their_neighbours_and_ends_stay_put() {
        let mut c = curve(&[p(0.0, 1.0), p(0.5, 0.5), p(1.0, 0.0)]);
        c.move_point(1, 2.0, 3.0);
        assert!(c[1].x < 1.0 && c[1].y == 1.0);
        c.move_point(0, 0.4, -1.0);
        assert_eq!((c[0].x, c[0].y), (0.0, 0.0));
    }

    #[test]
    fn inserting_keeps_the_curve_and_ends_cant_go() {
        let mut c = curve(&[line(0.0, 0.0, 0.0), p(1.0, 1.0)]);
        assert_eq!(c.insert(0.25), Some(1));
        assert!(close(c[1].y, 0.25) && c[1].linear);
        assert!(close(value(&c, 0.6), 0.6));
        assert_eq!(c.insert(0.2502), None, "too close to a point");
        assert_eq!(c.insert(1.5), None);
        assert!(!c.remove(0) && !c.remove(2));
        assert!(c.remove(1) && c.len() == 2);
    }
}
