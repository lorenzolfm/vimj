//! Where things are on screen. Each frame records what it draws where, so a
//! click can be traced back to what it landed on.

use crate::app::Focus;
use crate::editor::Field;
use ratatui::layout::{Position, Rect};
use std::time::{Duration, Instant};

/// Two clicks on the same thing this close together make a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    /// Behind a popup; a click there closes it.
    Backdrop,
    /// Inside a popup, but on nothing in particular.
    Popup,
    /// Somewhere in the sidebar or the main pane.
    Pane(Focus),
    Side(usize),
    Song(usize),
    /// The transition column of a song's row.
    Link(usize),
    Download(usize),
    /// The player's waveform, which spans the whole song.
    Seek,
    Toggle,
    Playing,
    Volume,
    Suggestion(usize),
    AddTo(usize),
    Field(Field),
    /// The arrows either side of the chosen field's value.
    Step(Field, i32),
    /// A song's lanes in the transition editor, and how many ms of that
    /// song each column shows.
    Lane(Field, f64),
    Preset(usize),
    Automatic,
}

#[derive(Default)]
pub struct Hits(Vec<(Rect, Target)>);

impl Hits {
    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn add(&mut self, area: Rect, target: Target) {
        if !area.is_empty() {
            self.0.push((area, target));
        }
    }

    /// What is under a cell: whatever was drawn there last.
    pub fn at(&self, column: u16, row: u16) -> Option<(Rect, Target)> {
        self.0.iter().rev().find(|(area, _)| area.contains(Position::new(column, row))).copied()
    }
}

/// `count` lines of `area` from line `first`, cut off at its bottom.
pub fn rows(area: Rect, first: usize, count: usize) -> Rect {
    let y = area.y.saturating_add(first.min(usize::from(u16::MAX)) as u16);
    Rect::new(area.x, y, area.width, count.min(usize::from(u16::MAX)) as u16).intersection(area)
}

/// `width` columns of `area` from column `first`, cut off at its right edge.
pub fn columns(area: Rect, first: usize, width: usize) -> Rect {
    let x = area.x.saturating_add(first.min(usize::from(u16::MAX)) as u16);
    Rect::new(x, area.y, width.min(usize::from(u16::MAX)) as u16, area.height).intersection(area)
}

/// Tells double clicks from single ones.
#[derive(Default)]
pub struct Clicks(Option<(Instant, Target)>);

impl Clicks {
    /// Records a click, and says whether it makes a double click.
    pub fn click(&mut self, target: Target) -> bool {
        let double = self.0.is_some_and(|(at, last)| last == target && at.elapsed() < DOUBLE_CLICK);
        // A third click starts over rather than making another double.
        self.0 = if double { None } else { Some((Instant::now(), target)) };
        double
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_click_lands_on_what_was_drawn_last() {
        let (screen, popup) = (Rect::new(0, 0, 80, 24), Rect::new(10, 5, 20, 4));
        let mut hits = Hits::default();
        hits.add(screen, Target::Song(3));
        hits.add(screen, Target::Backdrop);
        hits.add(popup, Target::Popup);
        hits.add(rows(popup, 1, 1), Target::AddTo(0));
        // Lines past the popup's bottom aren't drawn, so can't be clicked.
        hits.add(rows(popup, 4, 1), Target::AddTo(1));
        let at = |column, row| hits.at(column, row).map(|(_, target)| target);
        assert_eq!(at(0, 0), Some(Target::Backdrop));
        assert_eq!(at(10, 5), Some(Target::Popup));
        assert_eq!(at(29, 6), Some(Target::AddTo(0)));
        assert_eq!(at(12, 9), Some(Target::Backdrop));
        assert_eq!(at(80, 0), None);
    }

    #[test]
    fn double_clicks_need_the_same_target_twice() {
        let mut clicks = Clicks::default();
        assert!(!clicks.click(Target::Song(1)));
        assert!(!clicks.click(Target::Song(2)));
        assert!(clicks.click(Target::Song(2)));
        assert!(!clicks.click(Target::Song(2)), "a third click starts over");
    }
}
