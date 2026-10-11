//! How a tab comes in when the model shows it to you: a cut in one frame
//! would leave you wondering where the chat went. The content grows in
//! along a drifting wave, each cell passing through block glyphs before its
//! own character shows: tachyonfx's `evolve_into` and `coalesce` with a
//! wave pattern, done as the hero field is, since its fades need RGB.

use std::time::{Duration, Instant};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

const DURATION: Duration = Duration::from_millis(1000);
/// Thinnest to full, what a cell grows through before its character shows.
const BLOCKS: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];
/// How wide the front is, against the time one cell takes to come in.
const SPREAD: f32 = 1.5;
/// The share of a cell's own progress spent on blocks; the rest shows its
/// character dim.
const EVOLVE: f32 = 0.75;
/// Terminal cells are about twice as tall as wide, as in the hero field.
const ROW_SCALE: f32 = 1.8;

/// The content panel coming in along a wave.
#[derive(Debug)]
pub(super) struct Reveal {
    since: Instant,
}

impl Reveal {
    pub(super) fn new() -> Self {
        Self {
            since: Instant::now(),
        }
    }

    pub(super) fn running(&self) -> bool {
        self.since.elapsed() < DURATION
    }

    /// Draws over what the tab just drew in `area`.
    pub(super) fn draw(&self, buf: &mut Buffer, area: Rect) {
        let elapsed = self.since.elapsed();
        let t = elapsed.as_secs_f32();
        let p = t / DURATION.as_secs_f32();
        for y in 0..area.height {
            for x in 0..area.width {
                let cell = &mut buf[(area.x + x, area.y + y)];
                match stage(x, y, area.height, p, t) {
                    Stage::Blank => {
                        cell.set_char(' ');
                    }
                    Stage::Block(block) => {
                        cell.set_char(block)
                            .set_style(Style::new().add_modifier(Modifier::DIM));
                    }
                    Stage::Dim => {
                        cell.set_style(Style::new().add_modifier(Modifier::DIM));
                    }
                    Stage::Shown => {}
                }
            }
        }
    }
}

/// Where a cell is in coming in.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Stage {
    /// Not reached yet.
    Blank,
    Block(char),
    /// Its own character, dim.
    Dim,
    Shown,
}

/// The stage of the cell at (`x`, `y`) of an area `rows` tall, at progress
/// `p` (0 to 1) and `t` seconds in.
fn stage(x: u16, y: u16, rows: u16, p: f32, t: f32) -> Stage {
    // Mostly the wave, with some of the row, so it still reads as coming
    // in from the top.
    let delay = 0.6 * wave(x, y, t) + 0.4 * f32::from(y) / f32::from(rows.max(1));
    let q = p * (1.0 + SPREAD) - delay * SPREAD;
    if q <= 0.0 {
        Stage::Blank
    } else if q < EVOLVE {
        let index = (q / EVOLVE * BLOCKS.len() as f32) as usize;
        Stage::Block(BLOCKS[index.min(BLOCKS.len() - 1)])
    } else if q < 1.0 {
        Stage::Dim
    } else {
        Stage::Shown
    }
}

/// 0 to 1: a sine drifting one way averaged with a cosine drifting the
/// other, whose amplitude saws up and drops, so the front never settles.
fn wave(x: u16, y: u16, t: f32) -> f32 {
    let (x, y) = (f32::from(x), f32::from(y) * ROW_SCALE);
    let a = (x * 0.12 + y * 0.25 - t * 3.0).sin();
    let amplitude = 0.5 + 0.5 * (t * 0.8).fract();
    let b = (x * 0.05 - y * 0.15 + t * 2.0).cos() * amplitude;
    (0.5 + 0.25 * (a + b)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLS: u16 = 80;
    const ROWS: u16 = 24;

    fn area(p: f32, t: f32) -> Vec<Stage> {
        (0..ROWS)
            .flat_map(|y| (0..COLS).map(move |x| stage(x, y, ROWS, p, t)))
            .collect()
    }

    /// For each column, the first row not shown yet.
    fn front(p: f32, t: f32) -> Vec<u16> {
        (0..COLS)
            .map(|x| {
                (0..ROWS)
                    .find(|&y| stage(x, y, ROWS, p, t) != Stage::Shown)
                    .unwrap_or(ROWS)
            })
            .collect()
    }

    #[test]
    fn it_starts_blank_and_ends_shown() {
        assert!(area(0.0, 0.0).iter().all(|&s| s == Stage::Blank));
        assert!(area(1.0, 1.0).iter().all(|&s| s == Stage::Shown));
    }

    #[test]
    fn halfway_some_cells_are_shown_some_growing_and_some_blank() {
        let cells = area(0.5, 0.5);
        assert!(cells.contains(&Stage::Shown));
        assert!(cells.iter().any(|s| matches!(s, Stage::Block(_))));
        assert!(cells.contains(&Stage::Blank));
    }

    #[test]
    fn the_front_is_a_wave() {
        let front = front(0.5, 0.5);
        assert!(front.iter().any(|&row| row != front[0]), "{front:?}");
    }

    #[test]
    fn the_wave_drifts() {
        assert_ne!(front(0.5, 0.2), front(0.5, 0.7));
    }
}
