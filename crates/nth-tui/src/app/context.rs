//! Read-only projection of the main session for the context tab.

use nth_session::context_preview::Preview;

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
        self.context_view
            .update(Some(crate::context::format(&preview)));
    }

    /// Live snapshots describe committed history, never streaming fragments.
    pub(super) fn context_updated(&mut self, value: &std::sync::Arc<serde_json::Value>) {
        if let Ok(preview) = serde_json::from_value::<Preview>((**value).clone()) {
            self.context_view
                .update(Some(crate::context::format(&preview)));
        }
    }
}
