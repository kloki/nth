//! The chat pane: the history, where it is scrolled to, and how it draws.

mod render;
mod scroll;
mod transcript;

use std::path::PathBuf;

use nth_protocol::Event;
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    widgets::Paragraph,
};
use scroll::Scroll;
#[cfg(test)]
pub use transcript::Entry;
pub use transcript::Transcript;

use crate::theme::dim;

pub struct Chat {
    pub transcript: Transcript,
    scroll: Scroll,
    /// Viewport height from the last draw, for page-sized scrolling.
    height: usize,
    /// First line of the last full screen, from the last draw.
    max_top: usize,
}

impl Chat {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            transcript: Transcript::new(cwd),
            scroll: Scroll::default(),
            height: 0,
            max_top: 0,
        }
    }

    pub fn apply(&mut self, event: &Event) {
        self.transcript.apply(event);
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll.up(lines, self.max_top);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll.down(lines, self.max_top);
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.half_page());
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.half_page());
    }

    pub fn jump_top(&mut self) {
        self.scroll.jump_top(self.max_top);
    }

    pub fn jump_bottom(&mut self) {
        self.scroll.jump_bottom();
    }

    /// Lines hidden below the viewport while scrolled up; zero while following.
    pub fn lines_below(&self) -> usize {
        if self.scroll.is_following() {
            return 0;
        }
        self.max_top - self.scroll.top(self.max_top)
    }

    /// Draws the visible history, or `banner` centred while there is none.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, banner: &str) {
        let total = self.transcript.layout(area.width);
        self.height = usize::from(area.height);
        self.max_top = total.saturating_sub(self.height);

        if self.transcript.is_empty() {
            let middle = Rect {
                y: area.y + area.height / 2,
                height: area.height.min(1),
                ..area
            };
            frame.render_widget(
                Paragraph::new(banner)
                    .style(dim())
                    .alignment(Alignment::Center),
                middle,
            );
            return;
        }

        let top = self.scroll.top(self.max_top);
        frame.render_widget(
            Paragraph::new(self.transcript.visible(top, self.height)),
            area,
        );
    }

    fn half_page(&self) -> usize {
        (self.height / 2).max(1)
    }
}
