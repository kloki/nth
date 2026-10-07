//! The chat tab of the content panel: the history, where it is scrolled
//! to, and how it draws.

mod after_write;
mod render;
mod scroll;
mod transcript;

use std::path::PathBuf;

use nth_protocol::{Event, Message};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    widgets::{Paragraph, ScrollbarState},
};
use scroll::Scroll;
pub use transcript::{Entry, Transcript};

use crate::theme::dim;

pub struct Chat {
    pub transcript: Transcript,
    scroll: Scroll,
    /// Viewport height from the last draw, for page-sized scrolling.
    height: usize,
    /// First line of the last full screen, from the last draw.
    max_top: usize,
    /// Where the last draw put the history and which line it started at,
    /// so a click finds what it landed on.
    area: Rect,
    top: usize,
}

impl Chat {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            transcript: Transcript::new(cwd),
            scroll: Scroll::default(),
            height: 0,
            max_top: 0,
            area: Rect::default(),
            top: 0,
        }
    }

    /// A chat showing the history in `messages`, scrolled to its end.
    pub fn replay(cwd: PathBuf, messages: &[Message]) -> Self {
        let mut chat = Self::new(cwd.clone());
        chat.transcript = Transcript::replay(cwd, messages);
        chat
    }

    /// Problems found while setting up, such as an unreadable AGENTS.md.
    pub fn warn(&mut self, warnings: &[String]) {
        for warning in warnings {
            self.transcript.push_error(warning.clone());
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

    /// Where the view sits in the history while scrolled up; `None` while
    /// following the bottom, so the bar only shows when you have moved.
    pub fn scrollbar(&self) -> Option<ScrollbarState> {
        if self.scroll.is_following() {
            return None;
        }
        Some(
            ScrollbarState::new(self.max_top + 1)
                .position(self.scroll.top(self.max_top))
                .viewport_content_length(self.height),
        )
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
        self.area = area;
        self.top = top;
        frame.render_widget(
            Paragraph::new(self.transcript.visible(top, self.height)),
            area,
        );
    }

    /// The entry drawn at screen position (`column`, `row`) by the last draw.
    pub fn entry_at(&self, column: u16, row: u16) -> Option<&Entry> {
        let (line, _) = self.line_at(column, row)?;
        self.transcript.entry_at(line)
    }

    /// The web address drawn at screen position (`column`, `row`), if any.
    pub fn link_at(&self, column: u16, row: u16) -> Option<String> {
        let (line, column) = self.line_at(column, row)?;
        self.transcript.link_at(line, column)
    }

    /// The history line and the column into it under a screen position.
    fn line_at(&self, column: u16, row: u16) -> Option<(usize, usize)> {
        if !self.area.contains((column, row).into()) {
            return None;
        }
        let line = self.top + usize::from(row.checked_sub(self.area.y)?);
        Some((line, usize::from(column.checked_sub(self.area.x)?)))
    }

    fn half_page(&self) -> usize {
        (self.height / 2).max(1)
    }
}
