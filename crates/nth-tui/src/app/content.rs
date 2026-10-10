//! The content panel above the input panel: what you look at. It holds a
//! list of tabs, the chat always first, and shows one of them.

use std::collections::HashSet;

use nth_protocol::{MonitorId, Panel};
use nth_session::subagent::SubagentId;

use super::{App, input::Input};
use crate::{
    chat::Chat, diagnostics::Diagnostics, monitor::MonitorView, plan::PlanView,
    subagent::SubagentView, usage::UsageView,
};

/// A view the content panel can show. Each view's state lives on the app,
/// so it keeps up with the session while another view is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Tab {
    Chat,
    Diagnostics,
    /// A background command the model started; its view is on the app.
    Monitor(MonitorId),
    /// The plan file, open while there is one.
    Plan,
    /// A subagent the model delegated to; its view is on the app.
    Subagent(SubagentId),
    /// What the session spent.
    Usage,
}

impl Tab {
    /// The chat is where the session lives, so it is always there.
    fn closable(self) -> bool {
        self != Tab::Chat
    }

    /// What kind of tab it is, in front of its name in the header.
    pub(crate) fn icon(self) -> &'static str {
        match self {
            Tab::Chat => "›",
            Tab::Diagnostics => "●",
            Tab::Plan => "≡",
            // Their own headers start with these too.
            Tab::Monitor(_) => "$",
            Tab::Subagent(_) => "@",
            Tab::Usage => "∑",
        }
    }

    /// A finished tab turns back to plain once you have seen it, except
    /// the plan, whose approval stays a fact about it until it changes.
    fn fades(self) -> bool {
        self != Tab::Plan
    }
}

/// How a tab is doing, which the header shows as its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TabState {
    Idle,
    Working,
    Done,
    Failed,
    /// Waiting on an answer from you.
    NeedsYou,
}

impl From<Panel> for Tab {
    fn from(panel: Panel) -> Self {
        match panel {
            Panel::Chat => Tab::Chat,
            Panel::Diagnostics => Tab::Diagnostics,
            Panel::Plan => Tab::Plan,
        }
    }
}

/// The open tabs, in the order they were opened, and the one showing.
#[derive(Debug)]
pub(crate) struct Content {
    tabs: Vec<Tab>,
    active: usize,
    /// Finished tabs that have shown since they finished.
    seen: HashSet<Tab>,
}

impl Default for Content {
    fn default() -> Self {
        Self {
            tabs: vec![Tab::Chat],
            active: 0,
            seen: HashSet::new(),
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

    /// What the header shows for `tab` doing `state`: done turns idle once
    /// the tab has shown, until it is something else and finishes again.
    pub(super) fn shown_state(&mut self, tab: Tab, state: TabState) -> TabState {
        if state != TabState::Done || !tab.fades() {
            self.seen.remove(&tab);
            return state;
        }
        if tab == self.active() {
            self.seen.insert(tab);
        }
        if self.seen.contains(&tab) {
            TabState::Idle
        } else {
            state
        }
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

    /// Opens `tab` after the others without showing it, so whatever you are
    /// looking at stays.
    pub(super) fn add(&mut self, tab: Tab) {
        if !self.tabs.contains(&tab) {
            self.tabs.push(tab);
        }
    }

    /// Closes `tab` wherever it is, keeping the one showing if it is
    /// another, or showing the one before it.
    pub(super) fn remove(&mut self, tab: Tab) {
        let Some(i) = self.tabs.iter().position(|&t| t == tab) else {
            return;
        };
        if !tab.closable() {
            return;
        }
        self.tabs.remove(i);
        self.seen.remove(&tab);
        if i <= self.active {
            self.active = self.active.saturating_sub(1);
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
            let tab = self.tabs.remove(self.active);
            self.seen.remove(&tab);
            self.active -= 1;
        }
    }
}

impl App {
    /// The tab's name in the header, after its icon.
    pub(super) fn tab_label(&self, tab: Tab) -> String {
        match tab {
            Tab::Chat => "chat".into(),
            Tab::Diagnostics => "diagnostics".into(),
            Tab::Usage => "usage".into(),
            Tab::Plan => self.plan.label(),
            Tab::Monitor(id) => self
                .monitor_views
                .get(&id)
                .map_or_else(|| format!("monitor {id}"), MonitorView::label),
            Tab::Subagent(id) => self
                .subagent_views
                .get(&id)
                .map_or_else(|| format!("subagent {id}"), SubagentView::label),
        }
    }

    /// In front of the tab's name: what kind it is, or a subagent's
    /// spinner while its turn runs.
    pub(super) fn tab_icon(&self, tab: Tab) -> &'static str {
        match tab {
            Tab::Subagent(id) => self
                .subagent_views
                .get(&id)
                .map_or(tab.icon(), SubagentView::icon),
            _ => tab.icon(),
        }
    }

    /// How the tab is doing, before [`Content::shown_state`] fades what
    /// you have seen.
    pub(super) fn tab_state(&self, tab: Tab) -> TabState {
        match tab {
            // A question stops the turn until you answer, so it comes first.
            Tab::Chat if matches!(self.input, Input::Question(_)) => TabState::NeedsYou,
            Tab::Chat if self.is_busy() => TabState::Working,
            Tab::Chat => self.last_turn,
            Tab::Diagnostics | Tab::Usage => TabState::Idle,
            Tab::Plan => self.plan.state(),
            // Its tab closes once the process stops.
            Tab::Monitor(_) => TabState::Working,
            Tab::Subagent(id) => self
                .subagent_views
                .get(&id)
                .map_or(TabState::Idle, SubagentView::state),
        }
    }
}

/// What the keys and the wheel do to any view, so the app moves whichever
/// tab is showing without knowing which.
pub(crate) trait Scrollable {
    fn scroll_up(&mut self, lines: usize);
    fn scroll_down(&mut self, lines: usize);
    fn page_up(&mut self);
    fn page_down(&mut self);
    fn jump_top(&mut self);
    fn jump_bottom(&mut self);
}

impl Scrollable for Chat {
    fn scroll_up(&mut self, lines: usize) {
        Chat::scroll_up(self, lines);
    }

    fn scroll_down(&mut self, lines: usize) {
        Chat::scroll_down(self, lines);
    }

    fn page_up(&mut self) {
        Chat::page_up(self);
    }

    fn page_down(&mut self) {
        Chat::page_down(self);
    }

    fn jump_top(&mut self) {
        Chat::jump_top(self);
    }

    fn jump_bottom(&mut self) {
        Chat::jump_bottom(self);
    }
}

impl Scrollable for Diagnostics {
    fn scroll_up(&mut self, lines: usize) {
        Diagnostics::scroll_up(self, lines);
    }

    fn scroll_down(&mut self, lines: usize) {
        Diagnostics::scroll_down(self, lines);
    }

    fn page_up(&mut self) {
        Diagnostics::page_up(self);
    }

    fn page_down(&mut self) {
        Diagnostics::page_down(self);
    }

    fn jump_top(&mut self) {
        Diagnostics::jump_top(self);
    }

    fn jump_bottom(&mut self) {
        Diagnostics::jump_bottom(self);
    }
}

impl Scrollable for UsageView {
    fn scroll_up(&mut self, lines: usize) {
        UsageView::scroll_up(self, lines);
    }

    fn scroll_down(&mut self, lines: usize) {
        UsageView::scroll_down(self, lines);
    }

    fn page_up(&mut self) {
        UsageView::page_up(self);
    }

    fn page_down(&mut self) {
        UsageView::page_down(self);
    }

    fn jump_top(&mut self) {
        UsageView::jump_top(self);
    }

    fn jump_bottom(&mut self) {
        UsageView::jump_bottom(self);
    }
}

impl Scrollable for MonitorView {
    fn scroll_up(&mut self, lines: usize) {
        MonitorView::scroll_up(self, lines);
    }

    fn scroll_down(&mut self, lines: usize) {
        MonitorView::scroll_down(self, lines);
    }

    fn page_up(&mut self) {
        MonitorView::page_up(self);
    }

    fn page_down(&mut self) {
        MonitorView::page_down(self);
    }

    fn jump_top(&mut self) {
        MonitorView::jump_top(self);
    }

    fn jump_bottom(&mut self) {
        MonitorView::jump_bottom(self);
    }
}

impl Scrollable for SubagentView {
    fn scroll_up(&mut self, lines: usize) {
        SubagentView::scroll_up(self, lines);
    }

    fn scroll_down(&mut self, lines: usize) {
        SubagentView::scroll_down(self, lines);
    }

    fn page_up(&mut self) {
        SubagentView::page_up(self);
    }

    fn page_down(&mut self) {
        SubagentView::page_down(self);
    }

    fn jump_top(&mut self) {
        SubagentView::jump_top(self);
    }

    fn jump_bottom(&mut self) {
        SubagentView::jump_bottom(self);
    }
}

impl Scrollable for PlanView {
    fn scroll_up(&mut self, lines: usize) {
        PlanView::scroll_up(self, lines);
    }

    fn scroll_down(&mut self, lines: usize) {
        PlanView::scroll_down(self, lines);
    }

    fn page_up(&mut self) {
        PlanView::page_up(self);
    }

    fn page_down(&mut self) {
        PlanView::page_down(self);
    }

    fn jump_top(&mut self) {
        PlanView::jump_top(self);
    }

    fn jump_bottom(&mut self) {
        PlanView::jump_bottom(self);
    }
}

impl App {
    /// The view the content panel shows, to scroll it; `None` for a
    /// monitor tab whose view is gone.
    pub(super) fn active_view(&mut self) -> Option<&mut dyn Scrollable> {
        Some(match self.content.active() {
            Tab::Chat => &mut self.chat,
            Tab::Diagnostics => &mut self.diagnostics,
            Tab::Usage => &mut self.usage_view,
            Tab::Monitor(id) => self.monitor_views.get_mut(&id)?,
            Tab::Subagent(id) => self.subagent_views.get_mut(&id)?,
            Tab::Plan => &mut self.plan,
        })
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
    fn adding_keeps_the_tab_showing_and_removing_keeps_it_too() {
        let mut content = Content::default();
        content.open(Tab::Diagnostics);
        content.add(Tab::Monitor(1));
        content.add(Tab::Monitor(2));
        assert_eq!(content.active(), Tab::Diagnostics);

        content.remove(Tab::Monitor(1));
        assert_eq!(content.active(), Tab::Diagnostics);
        content.select(2);
        content.remove(Tab::Diagnostics);
        assert_eq!(content.active(), Tab::Monitor(2), "shifted with its tab");
        content.remove(Tab::Monitor(2));
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
