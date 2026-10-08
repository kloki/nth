//! What an empty chat shows: a procedural ASCII field that drifts with time
//! and ripples and brightens under the mouse. The maths is performative-ui's
//! `AsciiHero`; where its canvas has alpha, a terminal has dim, normal and
//! bold, so the spotlight steps through those.

use std::time::Duration;

use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Modifier, Style},
};

/// Sparsest to densest. Far shorter than the original's 70: on terminal
/// cells a fine ramp swaps nearly every glyph on every tick and flickers,
/// where ten steps let a cell hold its character for half a second.
const RAMP: &[u8] = b" .:-=+*#%@";
/// Terminal cells are about twice as tall as wide; distances to the pointer
/// count rows this much more, so the ripple comes out round.
const ROW_SCALE: f32 = 1.8;
const RIPPLE_STRENGTH: f32 = 1.4;
/// How far out the ripple's trough sits, in cells.
const RIPPLE_RADIUS: f32 = 6.0;
const SPOTLIGHT_RADIUS: f32 = 8.0;

/// The character at (`x`, `y`) of a `cols` by `rows` field `t` seconds in,
/// with the pointer at `pointer` in cells of the field, and how it is drawn.
pub fn cell(
    x: u16,
    y: u16,
    cols: u16,
    rows: u16,
    t: f32,
    pointer: Option<(f32, f32)>,
) -> (char, Style) {
    let (x, y) = (f32::from(x), f32::from(y));
    let nx = x / f32::from(cols.max(1)) * 2.0 - 1.0;
    let ny = y / f32::from(rows.max(1)) * 2.0 - 1.0;
    let bands = 0.5 + 0.5 * (nx * 6.0 + ny * 2.0).sin();
    let glow = 1.0 - (nx.hypot(ny) * 1.2).min(1.0);
    let base = 0.25 * bands + 0.55 * glow;
    let wave = 0.15 * (x * 0.18 + t * 1.4).sin() * (y * 0.22 - t * 1.1).cos();

    let mut style = Style::new().add_modifier(Modifier::DIM);
    let mut ripple = 0.0;
    if let Some((px, py)) = pointer {
        let (dx, dy) = (x - px, (y - py) * ROW_SCALE);
        let k = dx * dx + dy * dy;
        let r = k.sqrt();
        ripple =
            RIPPLE_STRENGTH * (-k / 80.0).exp() - 0.6 * (-(r - RIPPLE_RADIUS).powi(2) / 30.0).exp();
        let light = (-k / (2.0 * SPOTLIGHT_RADIUS * SPOTLIGHT_RADIUS)).exp();
        if light > 0.8 {
            style = Style::new().add_modifier(Modifier::BOLD);
        } else if light > 0.35 {
            style = Style::new();
        }
    }

    let value = (base + wave + ripple).clamp(0.0, 1.0);
    let index = (value * (RAMP.len() - 1) as f32) as usize;
    (char::from(RAMP[index]), style)
}

/// Fills `area` with the field `elapsed` after it started; `pointer` is
/// where the mouse last was on screen, ignored outside `area`.
pub fn draw(frame: &mut Frame, area: Rect, elapsed: Duration, pointer: Option<Position>) {
    let t = elapsed.as_secs_f32();
    let pointer = pointer
        .filter(|at| area.contains(*at))
        .map(|at| (f32::from(at.x - area.x), f32::from(at.y - area.y)));
    let buffer = frame.buffer_mut();
    for y in 0..area.height {
        for x in 0..area.width {
            let (symbol, style) = cell(x, y, area.width, area.height, t, pointer);
            if symbol == ' ' {
                continue;
            }
            buffer[(area.x + x, area.y + y)]
                .set_char(symbol)
                .set_style(style);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLS: u16 = 80;
    const ROWS: u16 = 24;

    fn density(symbol: char) -> usize {
        RAMP.iter()
            .position(|&c| char::from(c) == symbol)
            .expect("from the ramp")
    }

    fn field(t: f32, pointer: Option<(f32, f32)>) -> Vec<(char, Style)> {
        (0..ROWS)
            .flat_map(|y| (0..COLS).map(move |x| cell(x, y, COLS, ROWS, t, pointer)))
            .collect()
    }

    #[test]
    fn denser_in_the_middle_than_the_corners() {
        let (middle, _) = cell(COLS / 2, ROWS / 2, COLS, ROWS, 0.0, None);
        let (corner, _) = cell(0, 0, COLS, ROWS, 0.0, None);
        assert!(density(middle) > density(corner), "{middle:?} {corner:?}");
        assert_eq!(corner, ' ');
    }

    #[test]
    fn it_moves_with_time() {
        assert_ne!(field(0.0, None), field(1.0, None));
    }

    #[test]
    fn the_pointer_ripples_only_near_it() {
        let pointer = Some((10.0, 5.0));
        let near = |p| cell(12, 5, COLS, ROWS, 0.0, p).0;
        let far = |p| cell(70, 20, COLS, ROWS, 0.0, p).0;
        assert_ne!(near(pointer), near(None));
        assert_eq!(far(pointer), far(None));
    }

    #[test]
    fn the_pointer_brightens_what_is_under_it() {
        let pointer = Some((40.0, 12.0));
        let (_, under) = cell(40, 12, COLS, ROWS, 0.0, pointer);
        let (_, near) = cell(46, 12, COLS, ROWS, 0.0, pointer);
        let (_, far) = cell(70, 2, COLS, ROWS, 0.0, pointer);
        assert!(under.add_modifier.contains(Modifier::BOLD));
        assert!(!near.add_modifier.contains(Modifier::DIM), "{near:?}");
        assert!(far.add_modifier.contains(Modifier::DIM));
        let (_, untouched) = cell(40, 12, COLS, ROWS, 0.0, None);
        assert!(untouched.add_modifier.contains(Modifier::DIM));
    }
}
