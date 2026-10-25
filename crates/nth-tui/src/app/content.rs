//! The content panel above the input panel: what you look at. It holds a
//! list of tabs, the chat always first, and shows one of them; on a wide
//! terminal it shows the chat on the left and another tab on the right.

use std::collections::HashSet;

use nth_icons::icons;
use nth_protocol::{MonitorId, Panel, StoppedBy};
use nth_session::subagent::SubagentId;

use super::{App, input::Input};
use crate::{
    chat::Chat,
    file::{FileId, FileView},
    monitor::MonitorView,
    pager::Pager,
    subagent::SubagentView,
};

/// A view the content panel can show. Each view's state lives on the app,
/// so it keeps up with the session while another view is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Tab {
    Chat,
    Diagnostics,
    /// A file the model showed you; its view is on the app.
    File(FileId),
    /// The keyboard reference.
    Keymaps,
    /// A background command the model started; its view is on the app.
    Monitor(MonitorId),
    /// The plan file, open while there is one.
    Plan,
    /// A subagent the model delegated to; its view is on the app.
    Subagent(SubagentId),
    /// What the session spent.
    Usage,
    /// The orchestration's work plan, open while there is one.
    Work,
}

impl Tab {
    /// The chat is where the session lives, so it is always there.
    fn closable(self) -> bool {
        self != Tab::Chat
    }

    /// What kind of tab it is, in front of its name in the header.
    pub(crate) fn icon(self) -> &'static str {
        let tab = &icons().tab;
        match self {
            Tab::Chat => tab.chat,
            Tab::Diagnostics => tab.diagnostics,
            Tab::Plan => tab.plan,
            Tab::File(_) => tab.file,
            Tab::Keymaps => tab.keymaps,
            // A monitor's own header starts with its icon too.
            Tab::Monitor(_) => tab.monitor,
            Tab::Subagent(_) => tab.subagent,
            Tab::Usage => tab.usage,
            Tab::Work => tab.work,
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
            Panel::Work => Tab::Work,
        }
    }
}

/// What the content panel draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Panes {
    /// One tab fills it.
    Single(Tab),
    /// The chat on the left, this tab on the right.
    Split(Tab),
}

impl Panes {
    pub(crate) fn shows(self, tab: Tab) -> bool {
        match self {
            Panes::Single(shown) => shown == tab,
            Panes::Split(side) => tab == Tab::Chat || side == tab,
        }
    }
}

/// The open tabs, in the order they were opened, the one showing, and the
/// one the right pane shows when split.
#[derive(Debug)]
pub(crate) struct Content {
    tabs: Vec<Tab>,
    /// The focus: the tab the keys and the prompt act on.
    active: usize,
    /// The last tab other than the chat that had the focus, kept while the
    /// chat has it so the right pane does not empty.
    side: Option<Tab>,
    /// Finished tabs that have shown since they finished.
    seen: HashSet<Tab>,
}

impl Default for Content {
    fn default() -> Self {
        Self {
            tabs: vec![Tab::Chat],
            active: 0,
            side: None,
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

    /// What to draw: split when `split` (the terminal is wide enough) and
    /// there is another tab to put beside the chat.
    pub(crate) fn panes(&self, split: bool) -> Panes {
        match self.side {
            Some(side) if split => Panes::Split(side),
            _ => Panes::Single(self.active()),
        }
    }

    /// What the header shows for `tab` doing `state`: done turns idle once
    /// the tab has shown, until it is something else and finishes again.
    pub(super) fn shown_state(&mut self, tab: Tab, state: TabState, panes: Panes) -> TabState {
        if state != TabState::Done || !tab.fades() {
            self.seen.remove(&tab);
            return state;
        }
        if panes.shows(tab) {
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
                self.focus(i);
                false
            }
            None => {
                self.tabs.push(tab);
                self.focus(self.tabs.len() - 1);
                true
            }
        }
    }

    /// Opens `tab` after the others without showing it, so whatever you are
    /// looking at stays; the right pane takes it if it had nothing.
    pub(super) fn add(&mut self, tab: Tab) {
        if !self.tabs.contains(&tab) {
            self.tabs.push(tab);
            if self.side.is_none() && tab != Tab::Chat {
                self.side = Some(tab);
            }
        }
    }

    /// Gives the tab at `index` the focus, and the right pane unless it is
    /// the chat.
    fn focus(&mut self, index: usize) {
        self.active = index;
        let tab = self.tabs[index];
        if tab != Tab::Chat {
            self.side = Some(tab);
        }
    }

    /// After the tab at `index` closed: the right pane keeps its tab if it
    /// is still open, else takes the focused tab, else the nearest other.
    fn settle_side(&mut self, closed: Tab, index: usize) {
        if self.active() != Tab::Chat {
            self.side = Some(self.active());
        } else if self.side == Some(closed) {
            let (before, after) = self.tabs.split_at(index);
            self.side = before
                .iter()
                .rev()
                .chain(after)
                .copied()
                .find(|&tab| tab != Tab::Chat);
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
        let was_active = i == self.active;
        if i <= self.active {
            self.active = self.active.saturating_sub(1);
        }
        // Closing a tab out of sight leaves the focus where it was.
        if was_active || self.side == Some(tab) {
            self.settle_side(tab, i);
        }
    }

    /// Shows the next tab, from the last back to the chat.
    pub(super) fn next(&mut self) {
        self.focus((self.active + 1) % self.tabs.len());
    }

    /// Shows the previous tab, from the chat back to the last.
    pub(super) fn prev(&mut self) {
        self.focus((self.active + self.tabs.len() - 1) % self.tabs.len());
    }

    /// Shows the tab at `index`, counted from 0; there may be no such tab.
    pub(super) fn select(&mut self, index: usize) {
        if index < self.tabs.len() {
            self.focus(index);
        }
    }

    /// Gives `tab` the focus if it is open.
    pub(super) fn select_tab(&mut self, tab: Tab) {
        if let Some(i) = self.tabs.iter().position(|&t| t == tab) {
            self.focus(i);
        }
    }

    /// Closes the tab showing and shows the one before it, unless it is
    /// the chat.
    pub(super) fn close(&mut self) {
        if self.active().closable() {
            self.remove(self.active());
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
            Tab::Plan => self.plan.view.label(),
            Tab::Work => self.work.view.label(),
            Tab::File(id) => self
                .file_tabs
                .views
                .get(&id)
                .map_or_else(|| format!("file {id}"), FileView::label),
            Tab::Keymaps => "keymaps".into(),
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

    /// ctrl+w: stops the monitor showing, whose tab then closes, or the
    /// subagent showing, or closes its tab once stopped.
    pub(super) fn stop_content(&mut self) {
        match self.content.active() {
            Tab::Monitor(id) => {
                self.monitors.stop(id, StoppedBy::User);
            }
            Tab::Subagent(id) => match self.subagent_views.get(&id) {
                Some(view) if view.is_running() => self.interrupt_subagent(id),
                _ => self.close_content(),
            },
            _ => {}
        }
    }

    /// Closes the tab showing, unless it is a monitor or subagent still
    /// running, or the plan or the work plan, which show while there is one.
    pub(super) fn close_content(&mut self) {
        if self.content.active() == Tab::Work && self.work.view.exists() {
            self.hint = Some("the work tab stays while there is a plan".into());
            return;
        }
        let running = match self.content.active() {
            Tab::Monitor(id) => self.monitor_views.contains_key(&id),
            Tab::Subagent(id) => self
                .subagent_views
                .get(&id)
                .is_some_and(SubagentView::is_running),
            _ => false,
        };
        if running {
            self.hint = Some("still running · ctrl+w stops it first".into());
            return;
        }
        // A monitor's tab is only here while it runs, and a subagent's view
        // stays: continued, it opens again with its turns.
        self.content.close();
        self.forget_closed_files();
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
            Tab::Diagnostics | Tab::Usage | Tab::File(_) | Tab::Keymaps => TabState::Idle,
            Tab::Plan => self.plan.view.state(),
            Tab::Work => self.work.view.state(&self.live_work()),
            // Its tab closes once the process stops.
            Tab::Monitor(_) => TabState::Working,
            Tab::Subagent(id) => self
                .subagent_views
                .get(&id)
                .map_or(TabState::Idle, SubagentView::state),
        }
    }
}

/// What a read did to a tab that shows while its file does: the plan's
/// and the work plan's.
pub(super) enum Change {
    /// The file is new.
    Created,
    /// The file is there, maybe revised.
    Present,
    Gone,
}

impl App {
    /// Opens, keeps or closes `tab` after a read of its file. A new one is
    /// what you will want to see, and it comes in after a blank like a tab
    /// the model shows you; a revision opens the tab unfocused, so the
    /// reply the model is writing stays in view.
    pub(super) fn tab_read(&mut self, tab: Tab, change: Change) {
        match change {
            Change::Created => {
                self.content.open(tab);
                self.flash = Some(super::flash::Flash::new());
            }
            Change::Present => self.content.add(tab),
            Change::Gone => self.content.remove(tab),
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

impl Scrollable for Pager {
    fn scroll_up(&mut self, lines: usize) {
        Pager::scroll_up(self, lines);
    }

    fn scroll_down(&mut self, lines: usize) {
        Pager::scroll_down(self, lines);
    }

    fn page_up(&mut self) {
        Pager::page_up(self);
    }

    fn page_down(&mut self) {
        Pager::page_down(self);
    }

    fn jump_top(&mut self) {
        Pager::jump_top(self);
    }

    fn jump_bottom(&mut self) {
        Pager::jump_bottom(self);
    }
}

impl App {
    /// The view with the focus, to scroll it; `None` for a monitor tab
    /// whose view is gone.
    pub(super) fn active_view(&mut self) -> Option<&mut dyn Scrollable> {
        self.view_mut(self.content.active())
    }

    /// The view of `tab`, to scroll it; `None` for a tab whose view is gone.
    pub(super) fn view_mut(&mut self, tab: Tab) -> Option<&mut dyn Scrollable> {
        Some(match tab {
            Tab::Chat => &mut self.chat,
            Tab::Diagnostics => &mut self.diagnostics.pager,
            Tab::Usage => &mut self.usage_view.pager,
            Tab::Monitor(id) => &mut self.monitor_views.get_mut(&id)?.pager,
            Tab::Subagent(id) => &mut self.subagent_views.get_mut(&id)?.chat,
            Tab::Plan => &mut self.plan.view.pager,
            Tab::Work => &mut self.work.view.pager,
            Tab::File(id) => &mut self.file_tabs.views.get_mut(&id)?.pager,
            Tab::Keymaps => &mut self.keymaps.pager,
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
    fn the_side_keeps_its_tab_while_the_chat_has_the_focus() {
        let mut content = Content::default();
        assert_eq!(content.panes(true), Panes::Single(Tab::Chat), "only chat");

        content.open(Tab::Diagnostics);
        content.open(Tab::Usage);
        content.select(0);
        assert_eq!(content.active(), Tab::Chat);
        assert_eq!(content.panes(true), Panes::Split(Tab::Usage));
        assert_eq!(content.panes(false), Panes::Single(Tab::Chat), "narrow");

        content.remove(Tab::Usage);
        assert_eq!(content.panes(true), Panes::Split(Tab::Diagnostics));
        content.remove(Tab::Diagnostics);
        assert_eq!(content.panes(true), Panes::Single(Tab::Chat));
    }

    #[test]
    fn an_added_tab_fills_an_empty_side_only() {
        let mut content = Content::default();
        content.add(Tab::Monitor(1));
        assert_eq!(content.active(), Tab::Chat);
        assert_eq!(content.panes(true), Panes::Split(Tab::Monitor(1)));
        content.add(Tab::Monitor(2));
        assert_eq!(content.panes(true), Panes::Split(Tab::Monitor(1)));
    }

    #[test]
    fn closing_the_focused_tab_moves_the_side_with_the_focus() {
        let mut content = Content::default();
        content.open(Tab::Diagnostics);
        content.open(Tab::Usage);
        content.close();
        assert_eq!(content.active(), Tab::Diagnostics);
        assert_eq!(content.panes(true), Panes::Split(Tab::Diagnostics));
        content.close();
        assert_eq!(content.panes(true), Panes::Single(Tab::Chat));
    }

    #[test]
    fn prev_wraps_from_the_chat_to_the_last() {
        let mut content = Content::default();
        content.prev();
        assert_eq!(content.active(), Tab::Chat, "one tab");

        content.open(Tab::Diagnostics);
        content.open(Tab::Usage);
        content.select(0);
        content.prev();
        assert_eq!(content.active(), Tab::Usage);
        content.prev();
        assert_eq!(content.active(), Tab::Diagnostics);
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
