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

/// Moves point `i` to (`x`, `y`). The first and last points only move up and
/// down; the others stay between their neighbours.
pub fn move_point(points: &mut [Point], i: usize, x: f32, y: f32) {
    let last = points.len() - 1;
    let x = if i == 0 || i == last {
        points[i].x
    } else {
        x.clamp(points[i - 1].x + MIN_GAP, points[i + 1].x - MIN_GAP)
    };
    points[i].x = x;
    points[i].y = y.clamp(0.0, 1.0);
}

/// Adds a point on the curve at `x`, splitting the segment there into two of
/// the same kind, and returns where it went. `None` when `x` is outside the
/// curve or too close to a point already there.
pub fn insert(points: &mut Vec<Point>, x: f32) -> Option<usize> {
    let at = points.iter().position(|q| q.x > x)?;
    if at == 0 || x - points[at - 1].x < MIN_GAP || points[at].x - x < MIN_GAP {
        return None;
    }
    let y = value(points, x);
    let linear = points[at - 1].linear;
    points[at - 1].bend = 0.0;
    points.insert(at, Point { x, y, linear, bend: 0.0 });
    Some(at)
}

/// Takes point `i` out, unless it is the first or the last.
pub fn remove(points: &mut Vec<Point>, i: usize) -> bool {
    if i == 0 || i + 1 >= points.len() {
        return false;
    }
    points.remove(i);
    true
}

/// Switches the segment after point `i` between eased and straight.
pub fn toggle_linear(points: &mut [Point], i: usize) {
    if i + 1 < points.len() {
        points[i].linear = !points[i].linear;
        points[i].bend = 0.0;
    }
}

/// Bends the segment after point `i` by `by`, within what keeps it in 0..=1.
pub fn bend(points: &mut [Point], i: usize, by: f32) {
    if i + 1 >= points.len() {
        return;
    }
    let (c1, c2) = base_controls(points[i], points[i + 1]);
    let (low, high) = bend_range(c1, c2);
    points[i].bend = (points[i].bend + by).clamp(low, high);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
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
        let mut c = vec![line(0.0, 0.0, 0.0), p(1.0, 1.0)];
        bend(&mut c, 0, 5.0);
        assert!(close(c[0].bend, 0.5));
        assert!(close(value(&c, 0.5), 0.75));
        assert!((0..=20).all(|i| (0.0..=1.0).contains(&value(&c, i as f32 / 20.0))));
        bend(&mut c, 0, -10.0);
        assert!(close(value(&c, 0.5), 0.25));
        // An eased segment from 1 to 0 already touches both limits.
        let mut d = vec![p(0.0, 1.0), p(1.0, 0.0)];
        bend(&mut d, 0, 1.0);
        bend(&mut d, 0, -1.0);
        assert_eq!(d[0].bend, 0.0);
    }

    #[test]
    fn inner_points_stay_between_their_neighbours_and_ends_stay_put() {
        let mut c = vec![p(0.0, 1.0), p(0.5, 0.5), p(1.0, 0.0)];
        move_point(&mut c, 1, 2.0, 3.0);
        assert!(c[1].x < 1.0 && c[1].y == 1.0);
        move_point(&mut c, 0, 0.4, -1.0);
        assert_eq!((c[0].x, c[0].y), (0.0, 0.0));
    }

    #[test]
    fn inserting_keeps_the_curve_and_ends_cant_go() {
        let mut c = vec![line(0.0, 0.0, 0.0), p(1.0, 1.0)];
        assert_eq!(insert(&mut c, 0.25), Some(1));
        assert!(close(c[1].y, 0.25) && c[1].linear);
        assert!(close(value(&c, 0.6), 0.6));
        assert_eq!(insert(&mut c, 0.2502), None, "too close to a point");
        assert_eq!(insert(&mut c, 1.5), None);
        assert!(!remove(&mut c, 0) && !remove(&mut c, 2));
        assert!(remove(&mut c, 1) && c.len() == 2);
    }
}
