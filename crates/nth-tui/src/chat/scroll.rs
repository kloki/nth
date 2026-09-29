//! Sticky scrolling for the chat history: pinned to the bottom until you
//! scroll up, then held still while new lines arrive below.

#[derive(Debug)]
pub struct Scroll {
    /// First visible line, used only while not following.
    top: usize,
    follow: bool,
}

impl Default for Scroll {
    fn default() -> Self {
        Self {
            top: 0,
            follow: true,
        }
    }
}

impl Scroll {
    /// `max_top` is the first line of the last full screen.
    pub fn top(&self, max_top: usize) -> usize {
        if self.follow {
            max_top
        } else {
            self.top.min(max_top)
        }
    }

    pub fn is_following(&self) -> bool {
        self.follow
    }

    pub fn up(&mut self, lines: usize, max_top: usize) {
        self.top = self.top(max_top).saturating_sub(lines);
        self.follow = max_top == 0;
    }

    pub fn down(&mut self, lines: usize, max_top: usize) {
        self.top = self.top(max_top) + lines;
        self.follow = self.top >= max_top;
    }

    pub fn jump_top(&mut self, max_top: usize) {
        self.top = 0;
        self.follow = max_top == 0;
    }

    pub fn jump_bottom(&mut self) {
        self.follow = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_the_bottom_as_lines_arrive() {
        let scroll = Scroll::default();

        assert_eq!(scroll.top(5), 5);
        assert_eq!(scroll.top(9), 9);
    }

    #[test]
    fn scrolled_up_view_stays_put_as_lines_arrive() {
        let mut scroll = Scroll::default();
        scroll.up(3, 10);

        assert_eq!(scroll.top(10), 7);
        assert_eq!(scroll.top(50), 7);
        assert!(!scroll.is_following());
    }

    #[test]
    fn reaching_the_bottom_follows_again() {
        let mut scroll = Scroll::default();
        scroll.up(3, 10);
        scroll.down(5, 10);

        assert!(scroll.is_following());
        assert_eq!(scroll.top(20), 20);
    }

    #[test]
    fn nothing_to_scroll_keeps_following() {
        let mut scroll = Scroll::default();
        scroll.up(3, 0);

        assert!(scroll.is_following());
    }
}
