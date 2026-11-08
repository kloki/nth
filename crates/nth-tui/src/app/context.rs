//! The context tab: the main session's next request, rebuilt from the idle
//! session on the changes you make and, coalesced to the one-second tick,
//! on what reaches the inbox; followed live through a running turn's
//! snapshots.

use nth_protocol::{ContextPreview, ContextSnapshot};
use tokio::sync::watch;

use super::{App, Tab};
use crate::context::ContextView;

/// The context tab's view, the slot every turn publishes into, the running
/// turn's subscription and when the idle view is due a rebuild.
#[derive(Default)]
pub(super) struct ContextTab {
    pub(super) view: ContextView,
    /// Shared with every turn's tools through the `FrontEnd`.
    pub(super) slot: ContextPreview,
    /// Held only while a turn runs and the tab is open: the loop builds a
    /// snapshot, O(history), only while something holds one.
    live: Option<watch::Receiver<Option<ContextSnapshot>>>,
    /// What the idle preview lists changed since it was built: the inbox,
    /// or files on disk. Left to the tick, since a monitor or a subagent
    /// sends these per line and each rebuild projects the whole history.
    stale: bool,
    /// Whether the tab showed when last built or ticked, so the tick
    /// rebuilds one that came into view without an input event: a resize
    /// that splits the panel, or a tab that closed on its own.
    shown: bool,
    /// How often the idle session was projected, for the tests that pin
    /// when that happens.
    #[cfg(test)]
    pub(super) rebuilds: usize,
}

impl ContextTab {
    /// Follows the running turn's snapshots from the next one on; whether
    /// it was not following already.
    fn watch(&mut self) -> bool {
        let new = self.live.is_none();
        if new {
            self.live = Some(self.slot.subscribe());
        }
        new
    }

    #[cfg(test)]
    pub(super) fn watching(&self) -> bool {
        self.live.is_some()
    }

    /// The next snapshot the running turn publishes, or never without a
    /// subscription.
    pub(super) async fn next(&mut self) -> ContextSnapshot {
        let Some(live) = &mut self.live else {
            return std::future::pending().await;
        };
        loop {
            // The app owns the sender, so the slot never closes while it runs.
            if live.changed().await.is_err() {
                return std::future::pending().await;
            }
            // A turn starting empties the slot; there is nothing to show.
            if let Some(snapshot) = live.borrow_and_update().clone() {
                return snapshot;
            }
        }
    }

    /// Live snapshots describe committed history, never streaming fragments.
    pub(super) fn show(&mut self, snapshot: ContextSnapshot) {
        self.view.update(Some(snapshot));
    }
}

impl App {
    /// Rebuilds the idle preview while it shows, at once: for the changes
    /// you make, and the turn ending.
    pub(super) fn refresh_context(&mut self) {
        let shown = self.content.panes(self.wide).shows(Tab::Context);
        self.context_tab.shown = shown;
        if !shown {
            return;
        }
        // Mid-turn the live snapshots show it; whatever is stale stays so
        // for the turn's end, which rebuilds.
        let Some(mut session) = self.session.clone() else {
            return;
        };
        self.adopt_llm(&mut session);
        session.mode = self.mode;
        if session.agent != self.agent {
            session.set_agent(self.agent.clone());
        }
        if session.extra_dirs != self.extra_dirs {
            session.set_extra_dirs(self.extra_dirs.clone());
        }
        let preview = session.context_preview_for(&self.tools, &self.inbox, true);
        #[cfg(test)]
        {
            self.context_tab.rebuilds += 1;
        }
        self.context_tab.view.update(Some(preview.into()));
        self.context_tab.stale = false;
    }

    /// The idle preview is out of date; the next tick rebuilds it once.
    pub(super) fn context_stale(&mut self) {
        self.context_tab.stale = true;
    }

    /// The tick's share: one rebuild for everything marked stale since the
    /// last, or for the tab having come into view.
    pub(super) fn refresh_stale_context(&mut self) {
        let shown = self.content.panes(self.wide).shows(Tab::Context);
        if shown && (self.context_tab.stale || !self.context_tab.shown) {
            self.refresh_context();
        } else {
            self.context_tab.shown = shown;
        }
    }

    /// Before a turn's task starts: its first request publishes at once,
    /// so an open tab subscribes now. The slot is emptied here as well as
    /// by the turn, since the task may fill in a skill first, and the tab
    /// opened in between must not take the last turn's snapshot for this
    /// one's.
    pub(super) fn context_turn_starting(&mut self) {
        self.context_tab.slot.reset();
        if self.content.tabs().contains(&Tab::Context) {
            self.context_tab.watch();
        }
    }

    /// The tab opened or showed while a turn runs: it follows the turn,
    /// starting from what the turn published so far, since subscribing
    /// marks that seen. An empty slot leaves it waiting for the next step
    /// rather than on the preview from before the turn.
    pub(super) fn follow_running_turn(&mut self) {
        if self.context_tab.watch() {
            self.context_tab.view.update(self.context_tab.slot.get());
        }
    }

    /// The tab closed: a running turn stops building for it.
    pub(super) fn context_closed(&mut self) {
        self.context_tab.live = None;
    }

    /// The turn ended, through the loop or not: a panicked task never
    /// reached the loop's end, which forgets what its `context` tool
    /// asked for. What it published last is not kept: the end rebuilds a
    /// shown tab, and a hidden one is rebuilt when it shows again.
    pub(super) fn context_turn_ended(&mut self) {
        self.context_tab.live = None;
        self.context_tab.slot.reset();
    }
}
