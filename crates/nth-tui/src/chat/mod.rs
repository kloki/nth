//! The chat tab of the content panel: the history, where it is scrolled
//! to, and how it draws.

mod after_write;
mod render;
mod transcript;

use std::path::PathBuf;

use nth_protocol::{Event, Message};
use ratatui::{
    Frame,
    layout::Rect,
    widgets::{Paragraph, ScrollbarState},
};
/// Tests reach a tool row's state through the crate, where the module is
/// not visible.
#[cfg(test)]
pub use transcript::ToolState;
pub use transcript::{Entry, Transcript};

use crate::{pager::Pager, settings::ChatSettings};

pub struct Chat {
    pub transcript: Transcript,
    pub(crate) pager: Pager,
    /// Where the last draw put the history and which line it started at,
    /// so a click finds what it landed on.
    area: Rect,
    top: usize,
}

impl Chat {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            transcript: Transcript::new(cwd),
            pager: Pager::chat(),
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

    #[cfg(test)]
    pub fn scroll_up(&mut self, lines: usize) {
        self.pager.scroll_up(lines);
    }

    #[cfg(test)]
    pub fn jump_top(&mut self) {
        self.pager.jump_top();
    }

    pub fn jump_bottom(&mut self) {
        self.pager.jump_bottom();
    }

    /// Where the view sits in the history while scrolled up; `None` while
    /// following the bottom, so the bar only shows when you have moved.
    pub fn scrollbar(&self) -> Option<ScrollbarState> {
        if self.pager.is_following() {
            return None;
        }
        self.pager.scrollbar()
    }

    /// Draws the visible history; an empty chat draws nothing, leaving
    /// what shows in its place to the caller.
    pub fn draw(&mut self, frame: &mut Frame, area: Rect, settings: ChatSettings) {
        let total = self.transcript.layout(area.width, settings);
        let height = usize::from(area.height);
        let top = self.pager.layout(total, height);
        if self.transcript.is_empty() {
            return;
        }

        self.area = area;
        self.top = top;
        frame.render_widget(Paragraph::new(self.transcript.visible(top, height)), area);
    }

    /// The entry drawn at screen position (`column`, `row`) by the last draw.
    pub fn entry_at(&self, column: u16, row: u16) -> Option<&Entry> {
        let (line, _) = self.line_at(column, row)?;
        self.transcript.entry_at(line)
    }

    /// The web address drawn at screen position (`column`, `row`), if any,
    /// and where on screen it is drawn.
    pub fn link_at(&self, column: u16, row: u16) -> Option<(Rect, String)> {
        let (line, column) = self.line_at(column, row)?;
        let (columns, url) = self.transcript.link_at(line, column)?;
        let x = self.area.x.saturating_add(columns.start as u16);
        let width = (columns.len() as u16).min(self.area.right().saturating_sub(x));
        Some((Rect::new(x, row, width, 1), url))
    }

    /// Where on screen the entry at (`column`, `row`) shows: its rows the
    /// last draw put in view, the full width of the history.
    pub fn entry_area_at(&self, column: u16, row: u16) -> Option<Rect> {
        let (line, _) = self.line_at(column, row)?;
        let lines = self.transcript.entry_lines(line)?;
        let first = lines.start.max(self.top);
        let last = lines.end.min(self.top + usize::from(self.area.height));
        let y = self.area.y + (first - self.top) as u16;
        Some(Rect::new(
            self.area.x,
            y,
            self.area.width,
            (last - first) as u16,
        ))
    }

    /// The history line and the column into it under a screen position.
    fn line_at(&self, column: u16, row: u16) -> Option<(usize, usize)> {
        if !self.area.contains((column, row).into()) {
            return None;
        }
        let line = self.top + usize::from(row.checked_sub(self.area.y)?);
        Some((line, usize::from(column.checked_sub(self.area.x)?)))
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;
    use crate::subagent::SubagentView;

    #[test]
    fn child_chat_hit_targets_follow_its_scrolled_viewport() {
        let mut child = SubagentView::new("agent".into(), "task".into(), "/repo".into());
        for i in 0..8 {
            child.chat.transcript.push_user(format!("message {i}"));
        }
        child
            .chat
            .apply(&Event::TextDelta("[link](https://example.com)".into()));
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).unwrap();
        let area = Rect::new(3, 1, 30, 7);
        terminal
            .draw(|f| {
                child.draw(f, area, ChatSettings::default());
            })
            .unwrap();
        let link = (area.y..area.bottom())
            .find_map(|y| child.chat.link_at(area.x + 2, y))
            .expect("link at bottom");
        assert_eq!(link.1, "https://example.com");
        assert!(matches!(
            child.chat.entry_at(link.0.x, link.0.y),
            Some(Entry::Answer(_))
        ));
        assert!(
            child.chat.entry_at(area.x, area.y).is_none(),
            "header is not chat"
        );
        child.chat.pager.jump_top();
        terminal
            .draw(|f| {
                child.draw(f, area, ChatSettings::default());
            })
            .unwrap();
        assert!(child.chat.link_at(link.0.x, link.0.y).is_none());
        assert!(
            matches!(child.chat.entry_at(area.x, area.y + 2), Some(Entry::User(text)) if text == "message 0")
        );
    }
}
