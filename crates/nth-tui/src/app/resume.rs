//! Resuming a saved session: listing them in the session picker, reading
//! the chosen one, and switching the app over to it.

use std::sync::Arc;

use nth_context::Context;
use nth_session::{Session, Summary, store};

use super::{App, input::Input};
use crate::{chat::Chat, session_picker::SessionPicker, status};

impl App {
    /// Opens the picker and lists the saved sessions afresh, since every
    /// turn may have saved one.
    pub(super) fn open_session_picker(&mut self) {
        self.completion = None;
        let current = self.session_id();
        let mut picker = SessionPicker::new(current.unwrap_or_default());
        match &self.store {
            Some(store) if self.session_listing.is_none() => {
                let store = store.clone();
                self.session_listing = Some(tokio::spawn(async move { store.list().await }));
            }
            // The listing in flight fills this picker when it comes.
            Some(_) => {}
            None => picker.load(Err("sessions are not being saved".into())),
        }
        self.input = Input::SessionPicker(picker);
    }

    pub(super) fn sessions_listed(&mut self, sessions: Result<Vec<Summary>, store::Error>) {
        self.session_listing = None;
        if let Input::SessionPicker(picker) = &mut self.input {
            picker.load(sessions.map_err(|e| e.to_string()));
        }
    }

    /// Reads the highlighted session in the background. Mid-turn the
    /// session in use is in the turn task, so there is nothing to swap out
    /// yet, as with `/clear`.
    pub(super) fn choose_session(&mut self) {
        let Input::SessionPicker(picker) = &self.input else {
            return;
        };
        let Some(id) = picker.chosen() else {
            return;
        };
        if self.is_busy() {
            return;
        }
        self.input = Input::Prompt;
        if Some(id) == self.session_id() {
            return;
        }
        if let Some(store) = self.store.clone() {
            if let Some(loading) = self.session_loading.take() {
                loading.abort();
            }
            let paths = self.paths.clone();
            self.session_loading = Some(tokio::spawn(async move {
                let mut session = store.load(id).await?;
                // Read for the session's own directory, which may not be
                // the one nth started in.
                let context = Context::load(session.cwd.clone(), paths).await;
                session.set_context(Arc::new(context));
                Ok(session)
            }));
        }
    }

    pub(super) fn session_loaded(&mut self, session: Result<Session, store::Error>) {
        self.session_loading = None;
        match session {
            // A turn started while it was being read; switching now would
            // lose that turn's session when it ends.
            Ok(_) if self.is_busy() => {}
            Ok(session) => self.resume(session),
            Err(e) => self
                .chat
                .transcript
                .push_error(format!("could not resume: {e}")),
        }
    }

    /// Switches to `session`: its history, model and effort, and its
    /// working directory, so the paths it already talked about still hold.
    pub(super) fn resume(&mut self, mut session: Session) {
        session.max_steps = self.max_steps;
        self.context = session.context().clone();
        self.model = session.model.clone();
        self.effort = session.effort;
        self.cwd = session.cwd.clone();
        let home = std::env::var("HOME").ok();
        self.place = status::place(&self.cwd, home.as_deref());
        self.chat = Chat::replay(session.cwd.clone(), &session.messages);
        self.chat.warn(&session.context().warnings);
        self.usage = None;
        self.files.clear();
        self.session = Some(session);
        self.index_files();
        self.load_git();
    }

    fn session_id(&self) -> Option<uuid::Uuid> {
        self.session.as_ref().map(|s| s.id)
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::{AssistantMessage, Message};
    use nth_session::Store;

    use super::*;
    use crate::{
        app::{keys::Action, tests::app},
        chat::Entry,
    };

    #[tokio::test]
    async fn resuming_restores_history_model_and_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path());
        let mut saved = Session::new("kimi", "/elsewhere".into());
        saved.messages.push(Message::User("fix it".into()));
        saved.messages.push(Message::Assistant(AssistantMessage {
            text: "fixed".into(),
            ..Default::default()
        }));
        store.save(&saved).await.expect("saves");
        let mut app = app().with_store(store);

        app.prompt.insert_str("/resume");
        app.apply(Action::Submit);
        let listing = app.session_listing.take().expect("listing");
        app.sessions_listed(listing.await.expect("lists"));
        app.apply(Action::Submit);
        let loading = app.session_loading.take().expect("loading");
        app.session_loaded(loading.await.expect("loads"));

        assert!(matches!(app.input, Input::Prompt));
        assert_eq!(app.session_id(), Some(saved.id));
        assert_eq!(app.model, "kimi");
        assert_eq!(app.cwd, std::path::Path::new("/elsewhere"));
        assert_eq!(app.place, "/elsewhere");
        let entries: Vec<_> = app.chat.transcript.entries().cloned().collect();
        assert_eq!(
            entries,
            [Entry::User("fix it".into()), Entry::Answer("fixed".into())]
        );
    }

    #[tokio::test]
    async fn resuming_reads_instructions_for_the_sessions_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join("project");
        std::fs::create_dir_all(&project).expect("project dir");
        std::fs::write(project.join("AGENTS.md"), "Be brief.").expect("writes");
        let store = Store::at(dir.path().join("sessions"));
        let mut saved = Session::new("kimi", project.clone());
        saved.messages.push(Message::User("fix it".into()));
        store.save(&saved).await.expect("saves");
        let mut app = app().with_store(store);

        app.open_session_picker();
        let listing = app.session_listing.take().expect("listing");
        app.sessions_listed(listing.await.expect("lists"));
        app.apply(Action::Submit);
        let loading = app.session_loading.take().expect("loading");
        app.session_loaded(loading.await.expect("loads"));

        let session = app.session.as_ref().expect("resumed");
        let Message::System(prompt) = &session.messages[0] else {
            panic!("starts with the system prompt");
        };
        let expected = format!(
            "Instructions from: {}\nBe brief.\n",
            project.join("AGENTS.md").display()
        );
        assert!(prompt.ends_with(&expected), "{prompt}");
    }

    #[test]
    fn without_a_store_the_picker_says_so() {
        let mut app = app();
        let id = app.session_id();

        app.open_session_picker();
        app.apply(Action::Submit);

        assert!(matches!(app.input, Input::SessionPicker(_)));
        assert_eq!(app.session_id(), id);
        assert!(app.session_listing.is_none());
    }
}
