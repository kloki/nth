//! Scrolling a tab of pre-rendered lines by whole lines, which every tab
//! but the chats does. A chat pages by half a screen and draws only a
//! window of its transcript, so it keeps its own (`chat/scroll.rs`).

use ratatui::{
    Frame,
    layout::Rect,
    text::Line,
    widgets::{Paragraph, ScrollbarState},
};

/// Where a view is scrolled, as of its last draw.
#[derive(Debug, Default)]
pub struct Pager {
    /// The first line in view, and how far it can go.
    top: usize,
    max_top: usize,
    height: usize,
    /// Whether the newest line is kept in view as lines come, until you
    /// scroll up; `None` for a view that never does, so reaching its
    /// bottom never pins it there.
    follow: Option<bool>,
}

impl Pager {
    /// A pager that keeps the newest line in view, as a monitor's does.
    pub fn following() -> Self {
        Self {
            follow: Some(true),
            ..Self::default()
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.top = self.top.saturating_sub(lines);
        if let Some(follow) = &mut self.follow {
            *follow = false;
        }
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.top = (self.top + lines).min(self.max_top);
        if let Some(follow) = &mut self.follow {
            *follow = self.top == self.max_top;
        }
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.height.max(1));
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.height.max(1));
    }

    pub fn jump_top(&mut self) {
        self.scroll_up(self.top);
    }

    pub fn jump_bottom(&mut self) {
        self.top = self.max_top;
        if let Some(follow) = &mut self.follow {
            *follow = true;
        }
    }

    /// Where the view sits in its lines, while they are longer than the
    /// tab; `None` while they all fit.
    pub fn scrollbar(&self) -> Option<ScrollbarState> {
        (self.max_top > 0).then(|| {
            ScrollbarState::new(self.max_top + 1)
                .position(self.top)
                .viewport_content_length(self.height)
        })
    }

    /// Draws `lines` into `area` from where the view is scrolled, kept
    /// within them now that their length is known.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, lines: Vec<Line<'_>>) {
        self.height = usize::from(area.height);
        self.max_top = lines.len().saturating_sub(self.height);
        self.top = if self.follow == Some(true) {
            self.max_top
        } else {
            self.top.min(self.max_top)
        };
        let top = u16::try_from(self.top).unwrap_or(u16::MAX);
        frame.render_widget(Paragraph::new(lines).scroll((top, 0)), area);
    }

    #[cfg(test)]
    pub fn top(&self) -> usize {
        self.top
    }
}

/// A view's lines as drawn last and the width they were wrapped for, so a
/// frame renders them again only after they or the width changed.
#[derive(Debug, Default)]
pub struct Cache(Option<(usize, Vec<Line<'static>>)>);

impl Cache {
    /// What the view shows changed: render it again when next drawn.
    pub fn invalidate(&mut self) {
        self.0 = None;
    }

    /// The lines kept for `width`; `None` when there are none or they
    /// were wrapped for another width.
    pub fn take(&mut self, width: usize) -> Option<Vec<Line<'static>>> {
        match self.0.take() {
            Some((drawn, lines)) if drawn == width => Some(lines),
            _ => None,
        }
    }

    /// Keeps `lines`, wrapped for `width`, for the next draw.
    pub fn keep(&mut self, width: usize, lines: Vec<Line<'static>>) {
        self.0 = Some((width, lines));
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    /// Draws `count` lines into a tab four rows high.
    fn draw(pager: &mut Pager, count: usize) {
        let mut terminal = Terminal::new(TestBackend::new(20, 4)).expect("test backend");
        let lines: Vec<Line> = (0..count).map(|n| Line::from(n.to_string())).collect();
        terminal
            .draw(|frame| pager.render(frame, frame.area(), lines))
            .expect("draw");
    }

    #[test]
    fn a_default_pager_never_pins_after_scrolling_to_the_bottom() {
        let mut pager = Pager::default();
        draw(&mut pager, 10);
        pager.scroll_down(6);
        assert_eq!(pager.top(), 6, "at the bottom");

        draw(&mut pager, 20);
        assert_eq!(pager.top(), 6, "stays where it was as lines come");
        pager.jump_bottom();
        draw(&mut pager, 30);
        assert_eq!(pager.top(), 16, "nor after jumping there");
    }

    #[test]
    fn a_following_pager_pins_until_scrolled_up() {
        let mut pager = Pager::following();
        draw(&mut pager, 10);
        assert_eq!(pager.top(), 6, "the newest line in view");
        draw(&mut pager, 20);
        assert_eq!(pager.top(), 16);

        pager.scroll_up(1);
        draw(&mut pager, 30);
        assert_eq!(pager.top(), 15, "scrolled up, it stays");

        pager.scroll_down(11);
        draw(&mut pager, 40);
        assert_eq!(pager.top(), 36, "back at the bottom, it follows again");
    }
}
