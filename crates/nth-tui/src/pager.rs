//! Scrolling state for content tabs, including windowed chat transcripts.

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
    /// Chat's requested top, retained when a larger viewport clamps it.
    saved_top: usize,
    /// Whether the newest line is kept in view as lines come, until you
    /// scroll up; `None` for a view that never does, so reaching its
    /// bottom never pins it there.
    follow: Option<bool>,
    /// Chat keeps following when there is nothing to scroll.
    chat: bool,
}

impl Pager {
    /// A pager that keeps the newest line in view, as a monitor's does.
    pub fn following() -> Self {
        Self {
            follow: Some(true),
            ..Self::default()
        }
    }

    /// A following pager for a windowed chat transcript.
    pub fn chat() -> Self {
        Self {
            chat: true,
            ..Self::following()
        }
    }

    pub fn scroll_up(&mut self, lines: usize) {
        self.top = self.top.saturating_sub(lines);
        self.saved_top = self.top;
        if let Some(follow) = &mut self.follow {
            *follow = self.chat && self.max_top == 0;
        }
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.top = self.top.saturating_add(lines).min(self.max_top);
        self.saved_top = self.top;
        if let Some(follow) = &mut self.follow {
            *follow = self.top == self.max_top;
        }
    }

    pub fn page_up(&mut self) {
        self.scroll_up(self.page_size());
    }

    pub fn page_down(&mut self) {
        self.scroll_down(self.page_size());
    }

    fn page_size(&self) -> usize {
        if self.chat {
            (self.height / 2).max(1)
        } else {
            self.height.max(1)
        }
    }

    pub fn jump_top(&mut self) {
        self.scroll_up(self.top);
    }

    pub fn jump_bottom(&mut self) {
        self.top = self.max_top;
        self.saved_top = self.top;
        if let Some(follow) = &mut self.follow {
            *follow = true;
        }
    }

    /// Updates the viewport and returns the first visible line.
    pub fn layout(&mut self, total: usize, height: usize) -> usize {
        self.height = height;
        self.max_top = total.saturating_sub(height);
        if self.follow == Some(true) {
            self.top = self.max_top;
            self.saved_top = self.top;
        } else if self.chat {
            self.top = self.saved_top.min(self.max_top);
        } else {
            self.top = self.top.min(self.max_top);
        }
        self.top
    }

    pub fn is_following(&self) -> bool {
        self.follow == Some(true)
    }

    /// Where the view sits in its lines, while they are longer than the
    /// tab; `None` while they all fit.
    pub fn scrollbar(&self) -> Option<ScrollbarState> {
        (self.max_top > 0 || (self.chat && !self.is_following())).then(|| {
            ScrollbarState::new(self.max_top + 1)
                .position(self.top)
                .viewport_content_length(self.height)
        })
    }

    /// Draws `lines` into `area` from where the view is scrolled, kept
    /// within them now that their length is known.
    pub fn render(&mut self, frame: &mut Frame, area: Rect, lines: Vec<Line<'_>>) {
        self.layout(lines.len(), usize::from(area.height));
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

    #[test]
    fn chat_follows_until_moved_and_resumes_at_the_bottom() {
        let mut pager = Pager::chat();
        assert_eq!(pager.layout(14, 4), 10);
        pager.scroll_up(3);
        assert_eq!(pager.layout(54, 4), 7);
        assert!(!pager.is_following());
        pager.scroll_down(100);
        assert!(pager.is_following());
        assert_eq!(pager.layout(64, 4), 60);
    }

    #[test]
    fn chat_keeps_following_when_nothing_scrolls() {
        let mut pager = Pager::chat();
        pager.layout(2, 4);
        pager.scroll_up(3);
        assert!(pager.is_following());
        pager.jump_top();
        assert!(pager.is_following());
        assert_eq!(pager.layout(10, 4), 6);
    }

    #[test]
    fn pages_are_half_height_for_chat_and_full_height_for_other_tabs() {
        let mut chat = Pager::chat();
        let mut tab = Pager::default();
        chat.layout(30, 5);
        tab.layout(30, 5);
        chat.page_up();
        tab.page_down();
        assert_eq!(chat.top(), 23);
        assert_eq!(tab.top(), 5);
        chat.page_down();
        tab.page_up();
        assert_eq!(chat.top(), 25);
        assert_eq!(tab.top(), 0);
        chat.layout(30, 1);
        chat.page_up();
        assert_eq!(chat.top(), 28);
    }

    #[test]
    fn chat_restores_unfollowed_position_after_resize_and_shrink() {
        let mut pager = Pager::chat();
        assert_eq!(pager.layout(30, 5), 25);
        pager.page_up();
        assert_eq!(pager.layout(30, 10), 20);
        assert!(!pager.is_following());
        assert_eq!(pager.layout(30, 5), 23);
        assert_eq!(pager.layout(8, 10), 0);
        assert!(
            pager.scrollbar().is_some(),
            "unfollowed chat retains its bar"
        );
        assert_eq!(pager.layout(30, 5), 23);
        pager.jump_bottom();
        assert_eq!(pager.layout(30, 10), 20);
        assert!(pager.is_following());
    }

    #[test]
    fn chat_scrolls_from_clamped_position_after_resize() {
        let mut pager = Pager::chat();
        pager.layout(30, 5);
        pager.page_up();
        assert_eq!(pager.layout(30, 10), 20);
        pager.scroll_up(2);
        assert_eq!(pager.layout(30, 5), 18);

        assert_eq!(pager.layout(30, 15), 15);
        pager.scroll_down(1);
        assert!(pager.is_following());
        assert_eq!(pager.layout(30, 5), 25);
    }

    #[test]
    fn other_pagers_discard_clamped_positions() {
        for mut pager in [Pager::default(), Pager::following()] {
            pager.layout(30, 5);
            pager.jump_bottom();
            pager.scroll_up(2);
            assert_eq!(pager.layout(30, 10), 20);
            assert_eq!(pager.layout(30, 5), 20);
        }
    }

    #[test]
    fn zero_height_pages_and_following_remain_well_defined() {
        let mut chat = Pager::chat();
        assert_eq!(chat.layout(5, 0), 5);
        chat.page_up();
        assert_eq!(chat.top(), 4);
        assert_eq!(chat.layout(5, 2), 3);
        chat.page_down();
        assert!(chat.is_following());
        assert_eq!(chat.layout(6, 0), 6);

        let mut tab = Pager::default();
        tab.layout(5, 0);
        tab.page_down();
        assert_eq!(tab.top(), 1);
        assert!(tab.scrollbar().is_some());
        tab.layout(0, 0);
        assert!(tab.scrollbar().is_none());
    }
}
