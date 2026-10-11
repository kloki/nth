//! The sweep a tab comes in with when the model shows it to you: a cut in
//! one frame would leave you wondering where the chat went.

use std::time::{Duration, Instant};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

const DURATION: Duration = Duration::from_millis(600);

/// The content panel coming in from the top, the rows not reached yet dim.
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

    pub(super) fn draw(&self, buf: &mut Buffer, area: Rect) {
        let reached = reached(area.height, self.since.elapsed());
        let rest = Rect {
            y: area.y + reached,
            height: area.height - reached,
            ..area
        };
        buf.set_style(rest, Style::new().add_modifier(Modifier::DIM));
    }
}

/// How many of `height` rows the sweep has reached after `elapsed`.
fn reached(height: u16, elapsed: Duration) -> u16 {
    let done = elapsed.min(DURATION).as_millis() * u128::from(height) / DURATION.as_millis();
    // At most `height`, so it fits.
    done as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sweep_goes_from_the_top_to_the_bottom() {
        assert_eq!(reached(20, Duration::ZERO), 0);
        assert_eq!(reached(20, DURATION / 2), 10);
        assert_eq!(reached(20, DURATION), 20);
        assert_eq!(reached(20, DURATION * 2), 20);
    }
}
