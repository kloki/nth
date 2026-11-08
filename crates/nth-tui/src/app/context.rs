//! The context tab: the main session's next request, rebuilt from the idle
//! session on the events that change it and followed live through the
//! running turn's snapshots.

use nth_protocol::{ContextPreview, ContextSnapshot};
use tokio::sync::watch;

use super::{App, Tab};
use crate::context::ContextView;

/// The context tab's view, the slot every turn publishes into and the
/// running turn's subscription.
#[derive(Default)]
pub(super) struct ContextTab {
    pub(super) view: ContextView,
    /// Shared with every turn's tools through the `FrontEnd`.
    pub(super) slot: ContextPreview,
    /// Held only while a turn runs and the tab is open: the loop builds a
    /// snapshot, O(history), only while something holds one.
    live: Option<watch::Receiver<Option<ContextSnapshot>>>,
    /// How often the idle session was projected, for the tests that pin
    /// when that happens.
    #[cfg(test)]
    pub(super) rebuilds: usize,
}

impl ContextTab {
    /// Follows the running turn's snapshots from the next one on.
    fn watch(&mut self) {
        if self.live.is_none() {
            self.live = Some(self.slot.subscribe());
        }
    }

    /// Stops following, keeping what the turn published last: its end may
    /// win the loop's `select!` over its final snapshot.
    fn unwatch(&mut self) {
        let Some(mut live) = self.live.take() else {
            return;
        };
        if live.has_changed().unwrap_or(false)
            && let Some(snapshot) = live.borrow_and_update().clone()
        {
            self.view.update(Some(snapshot));
        }
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
    /// Rebuilds the idle preview while it shows. Called on the events that
    /// change what the next request would be, never on a timer: each one
    /// projects the whole history.
    pub(super) fn refresh_context(&mut self) {
        if !self.content.panes(self.wide).shows(Tab::Context) {
            return;
        }
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
    }

    /// Follows the turn's snapshots when the tab is open: called before a
    /// turn's task starts, since its first request publishes at once, and
    /// when the tab opens mid-turn.
    pub(super) fn watch_context(&mut self) {
        if self.content.tabs().contains(&Tab::Context) {
            self.context_tab.watch();
        }
    }

    /// Stops following the turn's snapshots: it ended, or the tab closed.
    pub(super) fn unwatch_context(&mut self) {
        self.context_tab.unwatch();
    }
}
