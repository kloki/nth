//! A tab the model shows you flashes white: a switch in one frame would
//! leave you wondering where the chat went.

use std::time::{Duration, Instant};

use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
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

    pub(super) fn draw(&self, buf: &mut Buffer, area: Rect) {
        buf.set_style(area, Style::new().bg(Color::White));
    }
}
