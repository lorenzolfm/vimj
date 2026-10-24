//! Colours and text formatting shared by the views.

use vimj_core::camelot::Camelot;
use vimj_core::{Analysis, Transition};
use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const GREEN: Color = Color::Rgb(30, 215, 96);
pub const PURPLE: Color = Color::Rgb(180, 155, 255);
pub const BLUE: Color = Color::Rgb(80, 155, 255);
pub const TEXT: Color = Color::Rgb(255, 255, 255);
pub const SUBTLE: Color = Color::Rgb(167, 167, 167);
pub const DIM: Color = Color::Rgb(100, 100, 100);
pub const FAINT: Color = Color::Rgb(60, 60, 60);
pub const HIGHLIGHT: Color = Color::Rgb(42, 42, 42);
pub const SELECTED: Color = Color::Rgb(36, 64, 48);
pub const PANEL: Color = Color::Rgb(18, 18, 18);
pub const WARN: Color = Color::Rgb(255, 196, 0);
pub const ERROR: Color = Color::Rgb(241, 94, 108);
/// The transition window, a shade lighter than the panel around it.
pub const WINDOW: Color = Color::Rgb(36, 36, 36);

// One colour for each kind of curve in the transition editor, as in Spotify's.
pub const VOLUME: Color = Color::Rgb(70, 170, 255);
pub const EQ: Color = Color::Rgb(255, 210, 50);
pub const FILTER: Color = Color::Rgb(245, 125, 225);
pub const EFFECT: Color = Color::Rgb(240, 240, 240);
pub const LOOPING: Color = Color::Rgb(255, 145, 60);

pub fn text() -> Style {
    Style::new().fg(TEXT)
}

pub fn subtle() -> Style {
    Style::new().fg(SUBTLE)
}

pub fn dim() -> Style {
    Style::new().fg(DIM)
}

pub fn accent() -> Style {
    Style::new().fg(GREEN)
}

pub fn bold(style: Style) -> Style {
    style.add_modifier(Modifier::BOLD)
}

/// One hue per position on the Camelot wheel.
pub fn camelot_color(c: Camelot) -> Color {
    let hue = f32::from(c.number - 1) * 30.0;
    let light = if c.minor { 0.62 } else { 0.72 };
    hsl(hue, 0.7, light)
}

pub fn hsl(h: f32, s: f32, l: f32) -> Color {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match h.rem_euclid(360.0) as u32 {
        0..60 => (c, x, 0.0),
        60..120 => (x, c, 0.0),
        120..180 => (0.0, c, x),
        180..240 => (0.0, x, c),
        240..300 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let to = |v: f32| ((v + m) * 255.0).round() as u8;
    Color::Rgb(to(r), to(g), to(b))
}

/// `a` taken `by` of the way to `b`.
pub fn mix(a: Color, b: Color, by: f32) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => {
            let at = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * by).round() as u8;
            Color::Rgb(at(r1, r2), at(g1, g2), at(b1, b2))
        }
        _ => a,
    }
}

pub fn energy_color(energy: f32) -> Color {
    hsl(120.0 - energy.clamp(0.0, 1.0) * 120.0, 0.65, 0.55)
}

pub fn camelot(analysis: Option<&Analysis>) -> Option<Camelot> {
    analysis.and_then(Analysis::camelot)
}

/// `3:41`
pub fn clock(ms: f64) -> String {
    let secs = (ms.max(0.0) / 1000.0) as u64;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// `3:41.2`
pub fn clock_tenths(ms: f64) -> String {
    format!("{}.{}", clock(ms), ((ms.max(0.0) % 1000.0) / 100.0) as u64)
}

/// `2h 51m`, `42 min`
pub fn long_duration(ms: u64) -> String {
    let minutes = ms / 60_000;
    if minutes >= 60 { format!("{}h {:02}m", minutes / 60, minutes % 60) } else { format!("{minutes} min") }
}

pub fn bars(n: u32) -> String {
    format!("{n} bar{}", if n == 1 { "" } else { "s" })
}

/// Vertical eighth blocks, empty to full.
pub const BLOCKS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

pub fn energy_glyph(energy: f32) -> char {
    BLOCKS[1 + ((energy.clamp(0.0, 1.0) * 7.0).round() as usize)]
}

/// `Blend · 8 bars`, prefixed with `Auto` when generated.
pub fn transition_summary(t: &Transition) -> String {
    let lead = if t.auto { "Auto · " } else { "" };
    format!("{lead}{} · {}", t.preset_label(), bars(t.bars))
}

/// Pads or truncates to exactly `width` terminal columns, with an ellipsis when cut.
pub fn fit(s: &str, width: usize) -> String {
    let used = s.width();
    if used <= width {
        let mut out = s.to_owned();
        out.extend(std::iter::repeat_n(' ', width - used));
        return out;
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out.extend(std::iter::repeat_n(' ', width - used - 1));
    out
}

pub fn plural(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_times() {
        assert_eq!(clock(221_900.0), "3:41");
        assert_eq!(clock_tenths(221_234.0), "3:41.2");
        assert_eq!(long_duration(10_260_000), "2h 51m");
        assert_eq!(long_duration(2_520_000), "42 min");
        assert_eq!(bars(1), "1 bar");
    }

    #[test]
    fn fits_text_to_width() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("日本語の歌", 5), "日本…");
    }
}
