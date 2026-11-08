//! Read-only projection of the main session for the context tab.

use nth_protocol::ContextSnapshot;
use nth_session::context_preview::Preview;
use tokio::sync::watch;

use super::{App, Tab};

impl App {
    /// Rebuild only while visible and idle, on an owned projection.
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
        self.context_view.update(Some(preview));
    }

    /// Live snapshots describe committed history, never streaming fragments.
    pub(super) fn context_updated(&mut self, value: &ContextSnapshot) {
        if let Ok(preview) = serde_json::from_value::<Preview>((**value).clone()) {
            self.context_view.update(Some(preview));
        }
    }
}

/// The next snapshot the running turn publishes, or never without one.
pub(super) async fn next_snapshot(
    live: &mut Option<watch::Receiver<Option<ContextSnapshot>>>,
) -> Option<ContextSnapshot> {
    let Some(live) = live else {
        return std::future::pending().await;
    };
    // The app owns the sender, so the slot never closes while it runs.
    if live.changed().await.is_err() {
        return std::future::pending().await;
    }
    live.borrow_and_update().clone()
}
