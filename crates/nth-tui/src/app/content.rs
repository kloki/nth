//! The content panel above the input panel: what you look at. It holds a
//! list of tabs, the chat always first, and shows one of them.

use nth_protocol::Panel;

/// A view the content panel can show. Each view's state lives on the app,
/// so it keeps up with the session while another view is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tab {
    Chat,
    Diagnostics,
}

impl Tab {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Tab::Chat => "chat",
            Tab::Diagnostics => "diagnostics",
        }
    }

    /// The chat is where the session lives, so it is always there.
    fn closable(self) -> bool {
        self != Tab::Chat
    }
}

impl From<Panel> for Tab {
    fn from(panel: Panel) -> Self {
        match panel {
            Panel::Chat => Tab::Chat,
            Panel::Diagnostics => Tab::Diagnostics,
        }
    }
}

/// The open tabs, in the order they were opened, and the one showing.
#[derive(Debug)]
pub(crate) struct Content {
    tabs: Vec<Tab>,
    active: usize,
}

impl Default for Content {
    fn default() -> Self {
        Self {
            tabs: vec![Tab::Chat],
            active: 0,
        }
    }
}

impl Content {
    pub(crate) fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub(crate) fn active(&self) -> Tab {
        self.tabs[self.active]
    }

    /// Shows `tab`, opening it after the others unless it is open already.
    /// Returns whether it was newly opened.
    pub(super) fn open(&mut self, tab: Tab) -> bool {
        match self.tabs.iter().position(|&t| t == tab) {
            Some(i) => {
                self.active = i;
                false
            }
            None => {
                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
                true
            }
        }
    }

    /// Shows the next tab, from the last back to the chat.
    pub(super) fn next(&mut self) {
        self.active = (self.active + 1) % self.tabs.len();
    }

    /// Shows the tab at `index`, counted from 0; there may be no such tab.
    pub(super) fn select(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.active = index;
        }
    }

    /// Closes the tab showing and shows the one before it, unless it is
    /// the chat.
    pub(super) fn close(&mut self) {
        if self.active().closable() {
            self.tabs.remove(self.active);
            self.active -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_an_open_tab_shows_it() {
        let mut content = Content::default();
        assert!(content.open(Tab::Diagnostics));
        content.select(0);
        assert!(!content.open(Tab::Diagnostics));
        assert_eq!(content.tabs(), [Tab::Chat, Tab::Diagnostics]);
        assert_eq!(content.active(), Tab::Diagnostics);
    }

    #[test]
    fn the_chat_never_closes() {
        let mut content = Content::default();
        content.close();
        assert_eq!(content.tabs(), [Tab::Chat]);

        content.open(Tab::Diagnostics);
        content.close();
        assert_eq!(content.tabs(), [Tab::Chat]);
        assert_eq!(content.active(), Tab::Chat);
    }

    #[test]
    fn next_wraps_and_select_ignores_missing_tabs() {
        let mut content = Content::default();
        content.next();
        assert_eq!(content.active(), Tab::Chat, "one tab");

        content.open(Tab::Diagnostics);
        content.next();
        assert_eq!(content.active(), Tab::Chat, "wraps");
        content.select(3);
        assert_eq!(content.active(), Tab::Chat);
        content.select(1);
        assert_eq!(content.active(), Tab::Diagnostics);
    }
}
