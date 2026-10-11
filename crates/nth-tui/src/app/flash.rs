//! A tab the model shows you comes in after a moment of blank screen: a
//! switch in one frame would leave you wondering where the chat went.

use std::time::{Duration, Instant};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Clear, Widget},
};

const DURATION: Duration = Duration::from_millis(150);

#[derive(Debug)]
pub(super) struct Flash {
    since: Instant,
}

impl Flash {
    pub(super) fn new() -> Self {
        Self {
            since: Instant::now(),
        }
    }

    pub(super) fn running(&self) -> bool {
        self.since.elapsed() < DURATION
    }

    /// Clears `area` to the terminal's own background.
    pub(super) fn draw(&self, buf: &mut Buffer, area: Rect) {
        Clear.render(area, buf);
    }
}
