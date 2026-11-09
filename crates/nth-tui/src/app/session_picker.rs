//! Resuming a saved session: listing them in the session picker, reading
//! the chosen one, and switching the app over to it.

use std::sync::Arc;

use nth_context::Context;
use nth_session::{Session, Summary, store};

use super::{App, TabState, input::Input, keys::Action, usage::Spent};
use crate::{chat::Chat, session_picker::SessionPicker, status};

pub(super) enum Loaded {
    Session(Box<Session>),
    Runtime(Box<super::PreparedRuntime>),
}

impl App {
    pub fn with_reload(mut self, reload: Option<super::Reload>) -> Self {
        self.reload = reload;
        self
    }

    /// A key while the session picker is open.
    pub(super) fn key_session_picker(&mut self, action: Action) {
        let Input::SessionPicker(picker) = &mut self.input else {
            return;
        };
        match action {
            Action::SelectNext => picker.next(),
            Action::SelectPrev => picker.prev(),
            Action::Insert(c) => picker.insert(c),
            Action::Backspace => picker.backspace(),
            Action::Submit => self.choose_session(),
            Action::ClearOrQuit if picker.clear_query() => {}
            Action::Interrupt | Action::ClearOrQuit | Action::LlmPicker => {
                self.input = Input::Prompt
            }
            _ => {}
        }
    }

    /// Opens the picker and lists the saved sessions afresh, since every
    /// turn may have saved one.
    pub(super) fn open_session_picker(&mut self) {
        self.completion = None;
        let mut picker = SessionPicker::new(self.session_id);
        match &self.store {
            Some(store) if !self.session_listing.is_running() => {
                let store = store.clone();
                self.session_listing
                    .start(|_| tokio::spawn(async move { store.list().await }));
            }
            // The listing in flight fills this picker when it comes.
            Some(_) => {}
            None => picker.load(Err("sessions are not being saved".into())),
        }
        self.input = Input::SessionPicker(picker);
    }

    pub(super) fn sessions_listed(&mut self, sessions: Result<Vec<Summary>, store::Error>) {
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
            self.hint = Some("a turn is running".into());
            return;
        }
        self.input = Input::Prompt;
        if id == self.session_id {
            return;
        }
        if let Some(store) = self.store.clone() {
            let paths = self.paths.clone();
            let reload = self.reload.clone();
            self.session_loading.start(|_| {
                tokio::spawn(async move {
                    let mut session = store.load(id).await?;
                    if let Some(reload) = reload {
                        return Ok(Loaded::Runtime(Box::new(reload(session).await?)));
                    }
                    // Read for the session's own directory, which may not be
                    // the one nth started in.
                    let context = Context::load(session.cwd.clone(), paths).await;
                    session.set_context(Arc::new(context));
                    Ok(Loaded::Session(Box::new(session)))
                })
            });
        }
    }

    pub(super) fn session_loaded(&mut self, session: anyhow::Result<Loaded>) {
        match session {
            // A turn started while it was being read; switching now would
            // lose that turn's session when it ends.
            Ok(_) if self.is_busy() => self.hint = Some("a turn is running".into()),
            Ok(Loaded::Session(session)) => self.resume(*session),
            Ok(Loaded::Runtime(runtime)) => self.install_runtime(*runtime),
            Err(e) => self
                .chat
                .transcript
                .push_error(format!("could not resume: {e:#}")),
        }
    }

    fn install_runtime(&mut self, runtime: super::PreparedRuntime) {
        let (session, warnings) = self.install(runtime);
        // `install` dropped the old provider's listing; until the new one
        // lists, the status bar has no window to show and nothing sizes the
        // next turn.
        self.list_llms();
        // `resume` hands these to every session from here on.
        self.env = super::configured(&session.env);
        self.resume(session);
        self.chat.warn(&warnings);
        // Shown in the split's side pane counts too: the reset look would
        // otherwise say `checking…` there until the tab is opened again.
        if self.panes().shows(super::Tab::Diagnostics) {
            self.diagnose();
        }
    }

    /// Switches to `session`: its history, model and effort, and its
    /// working directory, so the paths it already talked about still hold.
    pub(super) fn resume(&mut self, mut session: Session) {
        if matches!(self.input, Input::Search(_)) {
            self.input = Input::Prompt;
        }
        self.search_jump = None;
        session.env = self.env.clone();
        self.context = session.context().clone();
        self.session_since = session.created_at;
        self.session_id = session.id;
        // The session's model and effort are its mode's or its agent's now;
        // the rest keep what they had. An agent no longer found leaves the
        // session in its mode.
        super::mode::follow(&mut self.selection, &session);
        let gone = session
            .agent
            .as_ref()
            .filter(|_| self.selection.agent.is_none())
            .map(|gone| format!("agent {gone} is gone, back to {}", self.selection.mode));
        self.cwd = session.cwd.clone();
        let home = std::env::var("HOME").ok();
        self.place = status::place(&self.cwd, home.as_deref());
        self.extra_dirs = session.extra_dirs.clone();
        self.chat = Chat::replay(session.cwd.clone(), &session.messages);
        self.last_turn = TabState::Idle;
        self.chat.warn(&session.context().warnings);
        self.usage = None;
        self.spent = Spent::new(session.usage.clone());
        self.files.clear();
        self.plan.for_session(session.plan_path());
        self.work.for_session(session.work_dir());
        self.session = Some(session);
        self.refresh_context();
        self.left_session();
        // Set after leaving the old session: what that handed back to the
        // prompt matters less than an agent gone.
        if let Some(gone) = gone {
            self.hint = Some(gone.into());
        }
        self.index_files();
        self.load_git();
        self.load_pr();
    }
}

#[cfg(test)]
mod tests {
    use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
    use nth_protocol::{
        AssistantMessage, BoxError, Listing, Message, Provider, Request, StreamEvent,
    };
    use nth_session::Store;

    use super::*;
    use crate::{
        app::{
            keys::Action,
            tests::{app, runtime},
        },
        chat::Entry,
    };

    #[tokio::test]
    async fn resuming_restores_history_model_and_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path());
        let mut saved = Session::new("kimi", "/elsewhere".into());
        saved.extra_dirs = vec!["/added".into()];
        saved.messages.push(Message::User("fix it".into()));
        saved.messages.push(Message::Assistant(AssistantMessage {
            text: "fixed".into(),
            ..Default::default()
        }));
        store.save(&saved).await.expect("saves");
        let mut app = app().with_store(store);
        let configured = nth_protocol::Compaction {
            auto: false,
            ..nth_protocol::Compaction::default()
        };
        app.env.compaction = configured;

        app.prompt.insert_str("/resume");
        app.apply(Action::Submit);
        let sessions = app.session_listing.join().await.expect("lists");
        app.sessions_listed(sessions);
        app.apply(Action::Submit);
        let session = app.session_loading.join().await.expect("loads");
        app.session_loaded(session);

        assert!(matches!(app.input, Input::Prompt));
        assert_eq!(app.session_id, saved.id);
        assert_eq!(app.selection.current().model, "kimi");
        assert_eq!(app.cwd, std::path::Path::new("/elsewhere"));
        assert_eq!(app.place, "/elsewhere");
        assert_eq!(app.extra_dirs, [std::path::PathBuf::from("/added")]);
        assert_eq!(
            app.session.as_ref().expect("idle").env.compaction,
            configured,
            "the resumed session follows today's [context]"
        );
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
        let sessions = app.session_listing.join().await.expect("lists");
        app.sessions_listed(sessions);
        app.apply(Action::Submit);
        let session = app.session_loading.join().await.expect("loads");
        app.session_loaded(session);

        let session = app.session.as_ref().expect("resumed");
        let Message::System(prompt) = &session.messages[0] else {
            panic!("starts with the system prompt");
        };
        let expected = format!(
            "Instructions from: {}\nBe brief.\n",
            project.join("AGENTS.md").display()
        );
        // The built-in agents follow the instructions.
        assert!(prompt.contains(&expected), "{prompt}");
    }

    /// What `/resume` prepares with: the session on settings of its own,
    /// or the error a broken repository config gives.
    fn reload(fail: bool) -> crate::Reload {
        Arc::new(move |mut session: Session| {
            async move {
                if fail {
                    return Err(anyhow::anyhow!("invalid key").context("bad repository config"));
                }
                session.env.max_steps = 17;
                session.env.tool_timeout = None;
                session.env.compaction.auto = false;
                session.set_extra_dirs(vec!["/configured".into()]);
                Ok(crate::PreparedRuntime {
                    selection: nth_config::Selection::same(nth_protocol::Llm {
                        model: "new-default".into(),
                        effort: Default::default(),
                    }),
                    ..runtime(session, Arc::new(crate::app::tests::Scripted::idle()))
                })
            }
            .boxed()
        })
    }

    #[tokio::test]
    async fn runtime_is_installed_together_and_clear_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        let mut saved = Session::new("saved-model", dir.path().into());
        saved.messages.push(Message::User("hello".into()));
        store.save(&saved).await.unwrap();
        let mut app = app().with_store(store).with_reload(Some(reload(false)));
        let old_provider = app.provider.clone();
        app.open_session_picker();
        let listed = app.session_listing.join().await.unwrap();
        app.sessions_listed(listed);
        app.choose_session();
        let loaded = app.session_loading.join().await.unwrap();
        app.session_loaded(loaded);
        assert_eq!(app.session_id, saved.id);
        assert_eq!(app.selection.current().model, "saved-model");
        assert!(!Arc::ptr_eq(&app.provider, &old_provider));
        assert_eq!(app.env.max_steps, 17);
        assert_eq!(app.env.tool_timeout, None);
        assert!(!app.env.compaction.auto);
        assert_eq!(app.extra_dirs, [std::path::PathBuf::from("/configured")]);
        assert!(!app.checkers.as_ref().unwrap().rtk);
        app.start_fresh_session();
        let fresh = app.session.as_ref().unwrap();
        assert_eq!(fresh.env.max_steps, 17);
        assert_eq!(fresh.env.tool_timeout, None);
        assert_eq!(fresh.extra_dirs, [std::path::PathBuf::from("/configured")]);
    }

    /// A provider listing only `model`, to tell its listing from another's.
    struct Lists(&'static str);

    impl Provider for Lists {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            let listing = Listing::from(vec![crate::llm_picker::tests::model(self.0, false)]);
            async move { Ok(listing) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            async { Err("unused".into()) }.boxed()
        }
    }

    #[tokio::test]
    async fn resuming_into_another_runtime_lists_its_models() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        let mut saved = Session::new("new/model", dir.path().into());
        saved.messages.push(Message::User("hello".into()));
        store.save(&saved).await.unwrap();
        let reload: crate::Reload = Arc::new(|session: Session| {
            async move { Ok(runtime(session, Arc::new(Lists("new/model")))) }.boxed()
        });
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(runtime(session, Arc::new(Lists("old/model"))))
            .with_store(store)
            .with_reload(Some(reload));
        app.list_llms();
        app.open_session_picker();
        let listed = app.session_listing.join().await.unwrap();
        app.sessions_listed(listed);
        app.choose_session();
        let loaded = app.session_loading.join().await.unwrap();
        app.session_loaded(loaded);

        assert!(app.llm_listing.is_running(), "listed again at once");
        let listing = app.llm_listing.join().await.unwrap().unwrap();
        let names: Vec<_> = listing.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(names, ["new/model"], "by the new provider");
    }

    #[test]
    fn the_chat_starts_with_its_runtimes_warnings() {
        // As `/resume` shows the ones its runtime came with.
        let session = Session::new("glm", "/repo".into());
        let app = App::new(crate::PreparedRuntime {
            warnings: vec!["no model set, picked zen/glm".into()],
            ..runtime(session, Arc::new(crate::app::tests::Scripted::idle()))
        });

        assert!(
            app.chat
                .transcript
                .entries()
                .any(|entry| format!("{entry:?}").contains("picked zen/glm"))
        );
    }

    #[tokio::test]
    async fn loader_failure_keeps_the_old_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        let mut saved = Session::new("saved", dir.path().into());
        saved.messages.push(Message::User("hello".into()));
        store.save(&saved).await.unwrap();
        let mut app = app().with_store(store).with_reload(Some(reload(true)));
        let original = app.session_id;
        app.open_session_picker();
        let listed = app.session_listing.join().await.unwrap();
        app.sessions_listed(listed);
        app.choose_session();
        let loaded = app.session_loading.join().await.unwrap();
        app.session_loaded(loaded);
        assert_eq!(app.session_id, original);
        assert_eq!(app.selection.current().model, "glm");
        assert!(
            app.chat
                .transcript
                .entries()
                .any(|entry| format!("{entry:?}").contains("invalid key"))
        );
        assert!(
            app.chat
                .transcript
                .entries()
                .any(|entry| format!("{entry:?}").contains("bad repository config"))
        );
    }

    /// An app with the diagnostics tab open, after resuming a session that
    /// thought `pondering` and answered `hello`, its runtime prepared from
    /// `config`; with the folder its store saves in, kept for the test.
    async fn resumed_on(
        config: impl Fn() -> nth_config::Config + Send + Sync + 'static,
    ) -> (App, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path());
        let mut saved = Session::new("glm", dir.path().into());
        saved.messages.push(Message::User("hi".into()));
        saved.messages.push(Message::Assistant(AssistantMessage {
            text: "hello".into(),
            reasoning: "pondering".into(),
            ..Default::default()
        }));
        store.save(&saved).await.unwrap();
        let config = Arc::new(config);
        let reload: crate::Reload = Arc::new(move |session: Session| {
            let config = config.clone();
            async move {
                Ok(crate::PreparedRuntime {
                    config: config(),
                    ..runtime(session, Arc::new(crate::app::tests::Scripted::idle()))
                })
            }
            .boxed()
        });
        let mut app = app().with_store(store).with_reload(Some(reload));
        app.open_content(super::super::Tab::Diagnostics);
        app.open_session_picker();
        let listed = app.session_listing.join().await.unwrap();
        app.sessions_listed(listed);
        app.choose_session();
        let loaded = app.session_loading.join().await.unwrap();
        app.session_loaded(loaded);
        assert_eq!(app.session_id, saved.id);
        (app, dir)
    }

    /// The config `runtime` prepares, so a resume onto it changes nothing.
    fn unchanged() -> nth_config::Config {
        runtime(
            Session::new("glm", "/repo".into()),
            Arc::new(crate::app::tests::Scripted::idle()),
        )
        .config
    }

    /// Whether the chat on the left of a 100-column app shows `text`.
    fn chat_shows(app: &mut App, text: &str) -> bool {
        crate::app::tests::rows_of(app, 100)[2..]
            .iter()
            .any(|row| row.chars().take(50).collect::<String>().contains(text))
    }

    /// A repository's `.nth/config` cannot set `[chat]` or `[layout]`, so
    /// what changes them on a resume is the global config, edited since nth
    /// started.
    #[tokio::test]
    async fn resuming_onto_a_changed_config_takes_its_chat_and_layout() {
        let (mut app, _dir) = resumed_on(unchanged).await;
        assert!(
            !chat_shows(&mut app, "hello"),
            "the diagnostics tab fills the panel"
        );

        let split = || {
            let mut config = unchanged();
            config.layout.split_width = 1;
            config
        };
        let (mut app, _dir) = resumed_on(split).await;
        assert!(chat_shows(&mut app, "hello"), "beside the diagnostics tab");
        assert!(chat_shows(&mut app, "pondering"), "thinking shows");

        let (mut app, _dir) = resumed_on(move || {
            let mut config = split();
            config.chat.thinking = false;
            config
        })
        .await;
        assert!(chat_shows(&mut app, "hello"));
        assert!(!chat_shows(&mut app, "pondering"), "thinking is hidden");
    }

    #[test]
    fn only_a_changed_chat_config_starts_the_settings_over() {
        let mut app = app();
        // As `/settings` flips it for the run.
        app.settings.thinking = false;

        app.apply_config(unchanged());
        assert!(!app.settings.thinking, "the same [chat] keeps the flip");

        let mut config = unchanged();
        config.chat.tool_output = false;
        app.apply_config(config);
        assert!(app.settings.thinking, "a changed [chat] starts over");
        assert!(!app.settings.tool_output);
    }

    #[test]
    fn without_a_store_the_picker_says_so() {
        let mut app = app();
        let id = app.session_id;

        app.open_session_picker();
        app.apply(Action::Submit);

        assert!(matches!(app.input, Input::SessionPicker(_)));
        assert_eq!(app.session_id, id);
        assert!(!app.session_listing.is_running());
    }
}
