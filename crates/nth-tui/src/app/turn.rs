//! Running a turn: moving the session into a task, interrupting it, and
//! taking the session back when it ends.

use std::time::{Duration, Instant};

use nth_session::{CancellationToken, Session, store};
use tokio::task::JoinHandle;

use super::App;
use crate::command::Command;

/// What a turn task hands back: the session, how its turn ended, and
/// whether saving it afterwards worked.
pub(super) struct Ended {
    session: Session,
    result: Result<(), nth_session::Error>,
    saved: Result<(), store::Error>,
}

/// A turn in flight. Esc cancels it cooperatively so the session comes back;
/// aborting the task would drop the session with it.
pub(super) struct Running {
    pub(super) handle: JoinHandle<Ended>,
    cancel: CancellationToken,
}

impl App {
    pub fn is_busy(&self) -> bool {
        self.turn.is_some()
    }

    pub(super) fn submit(&mut self) {
        if let Some(command) = Command::parse(self.prompt.text()) {
            self.prompt.clear();
            self.run_command(command);
            return;
        }
        if self.is_busy() || self.prompt.text().trim().is_empty() {
            return;
        }
        let Some(mut session) = self.session.take() else {
            return;
        };
        // Picked in the model picker since the last turn, maybe mid-turn.
        if session.model != self.model {
            session.set_model(self.model.clone());
        }
        session.effort = self.effort;
        let text = self.prompt.take();
        // `/name args` runs a skill: the chat shows it as typed, and the
        // model gets the skill filled in.
        let skill = nth_context::skills::parse(&text, &self.context.skills)
            .map(|(skill, args)| (skill.clone(), args.to_string()));
        self.chat.transcript.push_user(text.clone());
        self.chat.jump_bottom();
        self.busy_since = Some(Instant::now());

        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let events = self.events_tx.clone();
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let store = self.store.clone();
        let handle = tokio::spawn(async move {
            let text = match skill {
                Some((skill, args)) => skill
                    .invoke(&args, &session.cwd)
                    .await
                    .map_err(nth_session::Error::Skill),
                None => Ok(text),
            };
            let result = match text {
                Ok(text) => {
                    session
                        .prompt(text, provider.as_ref(), &tools, &events, &token)
                        .await
                }
                Err(e) => Err(e),
            };
            // Saved however the turn ended, interrupted included: the
            // session is always valid to continue from.
            let saved = match &store {
                Some(store) => store.save(&session).await,
                None => Ok(()),
            };
            Ended {
                session,
                result,
                saved,
            }
        });
        self.turn = Some(Running { handle, cancel });
    }

    pub(super) fn interrupt(&mut self) {
        if let Some(running) = &self.turn {
            running.cancel.cancel();
        }
    }

    pub(super) fn end_turn(
        &mut self,
        Ended {
            session,
            result,
            saved,
        }: Ended,
    ) {
        // Events sent just before the task returned may still be queued, and
        // they belong above the footer.
        while let Ok(event) = self.events_rx.try_recv() {
            self.chat.apply(&event);
        }
        let elapsed = self
            .busy_since
            .take()
            .map_or(Duration::ZERO, |t| t.elapsed());
        let transcript = &mut self.chat.transcript;
        match result {
            Err(nth_session::Error::Interrupted) => transcript.interrupt(elapsed),
            result => {
                // The session's model ran the turn; `self.model` may have
                // been switched since.
                transcript.finish_turn(result.map_err(|e| e.to_string()), &session.model, elapsed)
            }
        }
        if let Err(e) = saved {
            transcript.push_error(format!("session not saved: {e}"));
        }
        self.session = Some(session);
        self.turn = None;
        self.index_files();
        self.load_git();
    }
}

impl Drop for App {
    fn drop(&mut self) {
        // Quitting mid-turn must not leave the agent running tools.
        if let Some(running) = &self.turn {
            running.handle.abort();
        }
        if let Some(indexing) = &self.indexing {
            indexing.cancel.cancel();
        }
        if let Some(listing) = &self.llm_listing {
            listing.abort();
        }
        if let Some(loading) = &self.git_loading {
            loading.abort();
        }
        if let Some(listing) = &self.session_listing {
            listing.abort();
        }
        if let Some(loading) = &self.session_loading {
            loading.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use crossterm::event::{KeyCode, KeyEvent};
    use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
    use nth_protocol::{BoxError, Message, ModelInfo, Provider, Request, StreamEvent};

    use super::*;
    use crate::chat::Entry;

    /// A provider that never answers, and records when its request is dropped.
    struct Hang(Arc<AtomicBool>);

    struct SetOnDrop(Arc<AtomicBool>);

    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl Provider for Hang {
        fn models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, BoxError>> {
            async { Ok(Vec::new()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            let guard = SetOnDrop(self.0.clone());
            async move {
                let _guard = guard;
                std::future::pending().await
            }
            .boxed()
        }
    }

    /// An app whose turn is running against [`Hang`], and the flag that
    /// shows when that turn's request was dropped.
    async fn busy_app() -> (App, Arc<AtomicBool>) {
        let dropped = Arc::new(AtomicBool::new(false));
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(
            session,
            Arc::new(Hang(dropped.clone())),
            Arc::new(Vec::new()),
        );
        app.prompt.insert_str("go");
        app.submit();
        tokio::task::yield_now().await;
        assert!(app.is_busy());
        (app, dropped)
    }

    #[tokio::test]
    async fn esc_interrupts_the_turn_and_keeps_the_session() {
        let (mut app, dropped) = busy_app().await;

        app.on_key(KeyEvent::from(KeyCode::Esc));
        let running = app.turn.take().expect("turn was running");
        let ended = running.handle.await.expect("turn task finished");
        app.end_turn(ended);

        assert!(!app.is_busy());
        assert!(dropped.load(Ordering::SeqCst), "request kept running");
        let session = app.session.as_ref().expect("session came back");
        assert_eq!(session.messages.last(), Some(&Message::User("go".into())));
        assert!(matches!(
            app.chat.transcript.entries().last(),
            Some(Entry::Interrupted { .. })
        ));
    }

    #[tokio::test]
    async fn a_skill_command_sends_the_filled_in_skill() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dropped = Arc::new(AtomicBool::new(false));
        let session = Session::new("glm", dir.path().to_path_buf())
            .with_context(crate::app::tests::with_fix_skill(dir.path()));
        let mut app = App::new(session, Arc::new(Hang(dropped)), Arc::new(Vec::new()));

        app.prompt.insert_str("/fix the build");
        app.submit();
        tokio::time::sleep(Duration::from_millis(100)).await;
        app.on_key(KeyEvent::from(KeyCode::Esc));
        let ended = app
            .turn
            .take()
            .expect("running")
            .handle
            .await
            .expect("ends");
        app.end_turn(ended);

        assert_eq!(
            app.chat.transcript.entries().next(),
            Some(&Entry::User("/fix the build".into()))
        );
        let session = app.session.as_ref().expect("session came back");
        let Some(Message::User(sent)) = session.messages.last() else {
            panic!("the prompt was sent");
        };
        assert!(
            sent.starts_with(
                "/fix the build\n\n<skill_content name=\"fix\">\n# Skill: fix\n\nFix the build.\n"
            ),
            "{sent}"
        );
        assert_eq!(session.title(), Some("/fix the build"));
    }

    #[tokio::test]
    async fn a_skill_that_cannot_be_read_fails_the_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let context = crate::app::tests::with_fix_skill(dir.path());
        std::fs::remove_file(dir.path().join(".agents/skills/fix/SKILL.md")).expect("removes");
        let session = Session::new("glm", dir.path().to_path_buf()).with_context(context);
        let mut app = App::new(
            session,
            Arc::new(Hang(Arc::default())),
            Arc::new(Vec::new()),
        );

        app.prompt.insert_str("/fix it");
        app.submit();
        let ended = app
            .turn
            .take()
            .expect("running")
            .handle
            .await
            .expect("ends");
        app.end_turn(ended);

        let Some(Entry::TurnError(e)) = app.chat.transcript.entries().last() else {
            panic!("the turn failed");
        };
        assert!(
            e.starts_with("could not run the skill: cannot read "),
            "{e}"
        );
    }

    #[tokio::test]
    async fn closing_the_picker_leaves_the_turn_running() {
        let (mut app, _) = busy_app().await;
        app.llms = Some(Vec::new());
        app.apply(crate::app::keys::Action::LlmPicker);

        app.on_key(KeyEvent::from(KeyCode::Esc));

        assert!(matches!(app.input, crate::app::input::Input::Prompt));
        let running = app.turn.as_ref().expect("still running");
        assert!(!running.cancel.is_cancelled());
    }

    #[tokio::test]
    async fn dropping_the_app_aborts_the_running_turn() {
        let (app, dropped) = busy_app().await;

        drop(app);
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert!(
            dropped.load(Ordering::SeqCst),
            "turn kept running after quit"
        );
    }
}
