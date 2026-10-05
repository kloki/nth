//! Running a turn: moving the session into a task, interrupting it, and
//! taking the session back when it ends.

use std::time::{Duration, Instant};

use nth_protocol::{Asker, FrontEnd, Screen};
use nth_session::{Session, store};

use super::App;
use crate::command::Command;

/// What a turn task hands back: the session, how its turn ended, and
/// whether saving it afterwards worked.
pub(super) struct Ended {
    session: Session,
    result: Result<(), nth_session::Error>,
    saved: Result<(), store::Error>,
    /// Ran a command you typed after `!`, not a model turn.
    shell: bool,
}

/// What Enter sends while a turn runs, held until it ends.
#[derive(Debug, Clone, PartialEq)]
pub enum Queued {
    Prompt(String),
    /// A command to run, typed after `!`.
    Shell(String),
}

impl Queued {
    /// As the prompt shows it: a command with its `!`.
    pub fn text(&self) -> String {
        match self {
            Queued::Prompt(text) => text.clone(),
            Queued::Shell(command) => format!("!{command}"),
        }
    }
}

impl App {
    pub fn is_busy(&self) -> bool {
        self.turn.is_running()
    }

    pub(super) fn submit(&mut self) {
        let shell = self.prompt.shell();
        if !shell && let Some(command) = Command::parse(self.prompt.text()) {
            self.prompt.clear();
            self.run_command(command);
            return;
        }
        if self.prompt.text().trim().is_empty() {
            return;
        }
        self.history.push(self.prompt.entry());
        self.save_history();
        let text = self.prompt.take();
        self.prompt.set_shell(false);
        // Sent when the running turn ends; the chat shows it only then, so
        // the transcript keeps the order the model saw.
        let next = match shell {
            true => Queued::Shell(text),
            false => {
                self.hold_notices = false;
                Queued::Prompt(text)
            }
        };
        if self.is_busy() {
            self.queue.push_back(next);
        } else {
            self.start(next);
        }
    }

    fn start(&mut self, next: Queued) {
        match next {
            Queued::Prompt(text) => self.start_turn(text),
            Queued::Shell(command) => self.start_shell(command),
        }
    }

    /// Runs `command` for you, without the model; the session keeps it as
    /// a bash call so the model sees it next turn. It runs as a turn does,
    /// so Esc stops it and what you send meanwhile waits for it.
    pub(super) fn start_shell(&mut self, command: String) {
        let Some(shell) = self.shell.clone() else {
            self.chat
                .transcript
                .push_error("no shell to run commands with".into());
            return;
        };
        let Some(mut session) = self.session.take() else {
            return;
        };
        self.chat.jump_bottom();
        self.busy_since = Some(Instant::now());
        let events = self.events_tx.clone();
        let store = self.store.clone();
        self.turn.start(|token| {
            tokio::spawn(async move {
                let result = session
                    .shell(command, shell.as_ref(), &events, &token)
                    .await;
                let saved = match &store {
                    Some(store) => store.save(&session).await,
                    None => Ok(()),
                };
                Ended {
                    session,
                    result,
                    saved,
                    shell: true,
                }
            })
        });
    }

    /// Starts a turn with `text` after what monitors said since the last
    /// turn; `text` is empty when only the monitors have something to say.
    pub(super) fn start_turn(&mut self, text: String) {
        if self.session.is_none() || (text.is_empty() && !self.monitors.has_notices()) {
            return;
        }
        let Some(mut session) = self.session.take() else {
            return;
        };
        let notices = self.monitors.take_notices();
        self.notices_due = None;
        // Picked in the model picker since the last turn, maybe mid-turn.
        if session.model != self.model {
            session.set_model(self.model.clone());
        }
        session.effort = self.effort;
        session.mode = self.mode;
        // `/name args` runs a skill: the chat shows it as typed, and the
        // model gets the skill filled in.
        let skill = nth_context::skills::parse(&text, &self.context.skills)
            .map(|(skill, args)| (skill.clone(), args.to_string()));
        if let Some(notices) = &notices {
            self.chat.transcript.push_user(notices.clone());
        }
        if !text.is_empty() {
            self.chat.transcript.push_user(text.clone());
        }
        self.chat.jump_bottom();
        self.plan.turn_started();
        self.busy_since = Some(Instant::now());

        let provider = self.provider.clone();
        let tools = self.tools.clone();
        let events = self.events_tx.clone();
        let store = self.store.clone();
        let front_end = FrontEnd {
            asker: Asker::new(self.asks_tx.clone()),
            screen: Screen::new(self.screen_tx.clone()),
            monitors: self.monitors.clone(),
        };
        self.turn.start(|token| {
            tokio::spawn(async move {
                let text = match skill {
                    Some((skill, args)) => skill
                        .invoke(&args, &session.cwd)
                        .await
                        .map_err(nth_session::Error::Skill),
                    None => Ok(text),
                };
                let text = text.map(|text| match notices {
                    Some(notices) if text.is_empty() => notices,
                    Some(notices) => format!("{notices}\n\n{text}"),
                    None => text,
                });
                let result = match text {
                    Ok(text) => {
                        session
                            .prompt(text, provider.as_ref(), &tools, &front_end, &events, &token)
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
                    shell: false,
                }
            })
        });
    }

    /// Esc cancels the turn cooperatively so the session comes back;
    /// aborting the task would drop the session with it.
    pub(super) fn interrupt(&mut self) {
        if self.is_busy() {
            self.interrupted = true;
        }
        self.turn.cancel();
    }

    pub(super) fn end_turn(
        &mut self,
        Ended {
            session,
            result,
            saved,
            shell,
        }: Ended,
    ) {
        // Events sent just before the task returned may still be queued, and
        // they belong above the footer.
        while let Ok(event) = self.events_rx.try_recv() {
            self.chat.apply(&event);
        }
        self.drop_asks();
        let elapsed = self
            .busy_since
            .take()
            .map_or(Duration::ZERO, |t| t.elapsed());
        let transcript = &mut self.chat.transcript;
        // Esc after the reply ended still comes back `Ok`, but it still
        // means stop.
        let send_next = result.is_ok() && !std::mem::take(&mut self.interrupted);
        match result {
            Err(nth_session::Error::Interrupted) => transcript.interrupt(elapsed),
            // The bash row says how a command went; no model answered.
            Ok(()) if shell => {}
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
        self.index_files();
        self.load_git();
        self.read_plan();
        // Notices that came after the model's last step go with the next
        // prompt, or wake it on their own.
        self.hold_notices = !send_next;
        match self.queue.pop_front() {
            Some(next) if send_next => self.start(next),
            Some(next) => {
                self.queue.push_front(next);
                self.unqueue();
            }
            None if send_next => self.start_turn(String::new()),
            None => {}
        }
    }

    /// After an interrupted or failed turn, the queued prompts go back into
    /// the prompt ahead of what is typed: sent prompts were written for a
    /// turn that went well, so they wait to be looked at again.
    fn unqueue(&mut self) {
        let mut parts: Vec<String> = self.queue.drain(..).map(|next| next.text()).collect();
        if !self.prompt.is_empty() {
            parts.push(self.prompt.take());
        }
        self.prompt.set(&parts.join("\n\n"));
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
    use nth_protocol::{
        BoxError, Message, ModelInfo, MonitorEvent, Provider, Request, Stream, StreamEvent,
    };

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
        let ended = app.turn.join().await.expect("turn task finished");
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
        let ended = app.turn.join().await.expect("ends");
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
        assert_eq!(session.title().as_deref(), Some("/fix the build"));
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
        let ended = app.turn.join().await.expect("ends");
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
    async fn a_sent_prompt_is_saved_to_the_history_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nth/prompt-history.jsonl");
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(
            session,
            Arc::new(Hang(Arc::default())),
            Arc::new(Vec::new()),
        )
        .with_history(crate::history::History::load(path.clone()).await);

        app.prompt.insert_str("two\nlines");
        app.submit();
        let saved = app.history_saving.join().await.expect("save finished");
        app.history_saved(saved);

        assert_eq!(
            std::fs::read_to_string(&path).expect("saved"),
            "\"two\\nlines\"\n"
        );
        let mut loaded = crate::history::History::load(path).await;
        assert_eq!(loaded.prev("").as_deref(), Some("two\nlines"));
    }

    /// A provider that answers every request with "ok".
    struct Answer;

    impl Provider for Answer {
        fn models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, BoxError>> {
            async { Ok(Vec::new()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            let reply =
                futures::stream::iter([Ok::<_, BoxError>(StreamEvent::TextDelta("ok".into()))]);
            async move { Ok(futures::StreamExt::boxed(reply)) }.boxed()
        }
    }

    fn send(app: &mut App, text: &str) {
        app.prompt.insert_str(text);
        app.submit();
    }

    async fn end(app: &mut App) {
        let ended = app.turn.join().await.expect("turn task finished");
        app.end_turn(ended);
    }

    fn last_user(app: &App) -> Option<&str> {
        app.chat
            .transcript
            .entries()
            .rev()
            .find_map(|entry| match entry {
                Entry::User(text) => Some(text.as_str()),
                _ => None,
            })
    }

    #[tokio::test]
    async fn enter_while_busy_queues_the_prompt() {
        let (mut app, _) = busy_app().await;

        send(&mut app, "next");

        assert!(app.prompt.is_empty());
        assert_eq!(app.queue, [Queued::Prompt("next".into())]);
        assert_eq!(last_user(&app), Some("go"), "shown only once sent");
        let token = app.turn.token().expect("still running");
        assert!(!token.is_cancelled());
    }

    #[tokio::test]
    async fn queued_prompts_run_one_turn_each_in_order() {
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(session, Arc::new(Answer), Arc::new(Vec::new()));
        send(&mut app, "one");
        send(&mut app, "two");
        send(&mut app, "three");

        end(&mut app).await;
        assert!(app.is_busy());
        assert_eq!(last_user(&app), Some("two"));
        assert_eq!(app.queue, [Queued::Prompt("three".into())]);

        end(&mut app).await;
        end(&mut app).await;
        assert!(!app.is_busy());
        assert!(app.queue.is_empty());
        let session = app.session.as_ref().expect("session came back");
        let sent: Vec<_> = session
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(sent, ["one", "two", "three"]);
    }

    #[tokio::test]
    async fn esc_gives_queued_prompts_back_instead_of_sending_them() {
        let (mut app, _) = busy_app().await;
        send(&mut app, "two");
        send(&mut app, "three");
        app.prompt.insert_str("typing");

        app.on_key(KeyEvent::from(KeyCode::Esc));
        end(&mut app).await;

        assert!(!app.is_busy());
        assert!(app.queue.is_empty());
        assert_eq!(app.prompt.text(), "two\n\nthree\n\ntyping");
    }

    #[tokio::test]
    async fn esc_after_the_reply_ended_still_holds_the_queue_back() {
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(session, Arc::new(Answer), Arc::new(Vec::new()));
        send(&mut app, "one");
        send(&mut app, "two");
        // Lets the turn finish `Ok` before Esc reaches it.
        tokio::time::sleep(Duration::from_millis(50)).await;

        app.interrupt();
        end(&mut app).await;

        assert!(!app.is_busy());
        assert_eq!(app.prompt.text(), "two");
        assert!(!app.interrupted, "the next turn starts clean");
    }

    /// Has a new monitor print `line`, as its task would, and lets the app
    /// hear of it.
    async fn monitor_says(app: &mut App, line: &str) {
        let m = app
            .monitors
            .register("ci", "watch")
            .await
            .expect("front-end");
        let output = MonitorEvent::Output {
            id: m.id,
            line: line.into(),
            stream: Stream::Stdout,
        };
        assert!(app.monitors.event(output).await);
        while let Ok(event) = app.monitor_rx.try_recv() {
            app.on_monitor(event);
        }
    }

    fn sent(app: &App) -> Vec<&str> {
        let session = app.session.as_ref().expect("session came back");
        session
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::User(text) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    const NOTICE: &str =
        "<monitor id=\"1\" description=\"ci\" log=\"/tmp/logs/1.log\">\nbuild failed\n</monitor>";

    fn app_logging_to_tmp(provider: Arc<dyn Provider>) -> App {
        let app = App::new(
            Session::new("glm", "/repo".into()),
            provider,
            Arc::new(Vec::new()),
        );
        app.monitors.set_log_dir("/tmp/logs".into());
        app
    }

    #[tokio::test]
    async fn notices_wake_an_idle_agent() {
        let mut app = app_logging_to_tmp(Arc::new(Answer));
        monitor_says(&mut app, "build failed").await;
        assert!(app.notices_due.is_some(), "waits for more lines first");

        app.notices_due();
        assert!(app.is_busy());
        end(&mut app).await;

        assert_eq!(sent(&app), [NOTICE]);
        assert!(matches!(
            app.chat.transcript.entries().next(),
            Some(Entry::Notice(notice)) if notice.lines == 1
        ));
        assert_eq!(last_user(&app), None, "nothing typed");
    }

    #[tokio::test]
    async fn notices_during_a_turn_follow_it() {
        let mut app = app_logging_to_tmp(Arc::new(Answer));
        send(&mut app, "one");
        monitor_says(&mut app, "build failed").await;
        app.notices_due();

        end(&mut app).await;
        assert!(app.is_busy(), "the notices' own turn");
        end(&mut app).await;

        assert_eq!(sent(&app), ["one", NOTICE]);
    }

    #[tokio::test]
    async fn after_esc_notices_wait_for_the_next_prompt() {
        let (mut app, _) = busy_app().await;
        app.monitors.set_log_dir("/tmp/logs".into());
        app.on_key(KeyEvent::from(KeyCode::Esc));
        end(&mut app).await;

        monitor_says(&mut app, "build failed").await;
        app.notices_due();
        assert!(!app.is_busy(), "you stopped the agent");

        send(&mut app, "fix it");
        app.on_key(KeyEvent::from(KeyCode::Esc));
        end(&mut app).await;
        // The interrupted "go" was never answered, so the next prompt joins
        // it rather than following it as a second user message.
        assert_eq!(sent(&app), [format!("go\n\n{NOTICE}\n\nfix it")]);
        assert_eq!(last_user(&app), Some("fix it"));
    }

    #[tokio::test]
    async fn clear_stops_the_monitors() {
        let mut app = app_logging_to_tmp(Arc::new(Answer));
        monitor_says(&mut app, "build failed").await;

        app.run_command(Command::Clear);

        assert_eq!(app.monitors.running(), 0);
        assert!(!app.monitors.has_notices());
    }

    #[tokio::test]
    async fn closing_the_picker_leaves_the_turn_running() {
        let (mut app, _) = busy_app().await;
        app.llms = Some(Vec::new());
        app.apply(crate::app::keys::Action::LlmPicker);

        app.on_key(KeyEvent::from(KeyCode::Esc));

        assert!(matches!(app.input, crate::app::input::Input::Prompt));
        let token = app.turn.token().expect("still running");
        assert!(!token.is_cancelled());
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

    /// An idle app in `dir` that runs commands with the real bash.
    fn shell_app(dir: &std::path::Path) -> App {
        let session = Session::new("glm", dir.to_path_buf());
        App::new(session, Arc::new(Answer), Arc::new(Vec::new()))
            .with_shell(Arc::new(nth_tools::Bash::untimed(Default::default())))
    }

    fn command(app: &mut App, text: &str) {
        app.apply(crate::app::keys::Action::Insert('!'));
        send(app, text);
    }

    #[tokio::test]
    async fn a_command_runs_without_the_model_and_lands_in_the_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = shell_app(dir.path());

        command(&mut app, "echo hi");
        assert!(app.is_busy());
        assert!(!app.prompt.shell(), "the prompt is back to the mode");
        end(&mut app).await;

        let entries: Vec<_> = app.chat.transcript.entries().collect();
        assert!(
            matches!(&entries[..], [Entry::Tool { call, output, .. }] if call.name == "bash" && output == &["hi"]),
            "{entries:?}"
        );
        let session = app.session.as_ref().expect("session came back");
        assert!(matches!(
            session.messages.last(),
            Some(Message::ToolResult { content, .. }) if content == "hi\n"
        ));
        assert_eq!(app.history.prev("").as_deref(), Some("!echo hi"));
    }

    #[tokio::test]
    async fn a_command_sent_while_busy_runs_after_the_turn() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = shell_app(dir.path());
        send(&mut app, "hello");
        command(&mut app, "echo later");
        assert_eq!(app.queue, [Queued::Shell("echo later".into())]);

        end(&mut app).await;
        assert!(app.is_busy(), "the command runs next");
        end(&mut app).await;

        let session = app.session.as_ref().expect("session came back");
        assert!(matches!(
            session.messages.last(),
            Some(Message::ToolResult { content, .. }) if content == "later\n"
        ));
    }

    #[tokio::test]
    async fn esc_stops_a_running_command() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = shell_app(dir.path());
        command(&mut app, "sleep 30");
        tokio::task::yield_now().await;

        app.apply(crate::app::keys::Action::Interrupt);
        end(&mut app).await;

        assert!(!app.is_busy());
        assert!(matches!(
            app.chat.transcript.entries().last(),
            Some(Entry::Interrupted { .. })
        ));
    }
}
