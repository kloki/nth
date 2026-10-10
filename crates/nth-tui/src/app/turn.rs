//! Running a turn: moving the session into a task, interrupting it, and
//! taking the session back when it ends.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use nth_notify::Event;
use nth_protocol::{Asker, FrontEnd, Screen};
use nth_session::{Session, plan, store};
use tokio::task::JoinError;

use super::{App, NOTICE_DELAY, TabState};
use crate::{command::Command, status};

/// What a turn task hands back: the session, how its turn ended, and
/// whether saving it afterwards worked.
pub(super) struct Ended {
    session: Session,
    result: Result<(), nth_session::Error>,
    saved: Result<(), store::Error>,
    /// Ran a command you typed after `!`, not a model turn.
    shell: bool,
}

/// What is sent while a turn runs. A plain prompt is not held: it steers
/// into the running turn through the model's inbox (see [`App::send`]);
/// the rest waits for the turn to end. What the model gets for the plan
/// ones is rendered only then: it is written for the model, so it never
/// goes back into the prompt.
#[derive(Debug, Clone, PartialEq)]
pub enum Queued {
    Prompt(String),
    /// A command to run, typed after `!`.
    Shell(String),
    /// `/approve`: the plan's approval.
    Approve,
    /// The plan as ctrl+g found it and as you left it.
    PlanEdits {
        original: String,
        edited: String,
    },
}

impl Queued {
    /// As the status bar names it: a prompt as typed, a command with its
    /// `!`, and a word for what goes to the model as a reminder or a diff.
    pub fn label(&self) -> String {
        match self {
            Queued::Prompt(text) => text.clone(),
            Queued::Shell(command) => format!("!{command}"),
            Queued::Approve => "/approve".into(),
            Queued::PlanEdits { .. } => "plan edits".into(),
        }
    }
}

impl App {
    pub fn is_busy(&self) -> bool {
        self.turn.is_running()
    }

    pub(super) fn submit(&mut self) {
        let shell = self.prompt.shell();
        if !shell
            && let Some((command, arg)) = Command::invocation(self.prompt.text())
            && let arg = arg.to_string()
        {
            self.prompt.clear();
            match (command, arg.is_empty()) {
                // `/add-dir <dir>` is the one command with an argument,
                // and it runs without a turn.
                (Command::AddDir, false) => self.add_dir(&arg),
                (command, _) => self.run_command(command),
            }
            return;
        }
        if self.prompt.text().trim().is_empty() {
            return;
        }
        // Kept in the prompt rather than sent to nobody.
        if let Some(id) = self.showing_subagent()
            && !shell
            && self.subagents.describe(id).is_none()
        {
            self.hint = Some("this subagent is gone".into());
            return;
        }
        self.history.push(self.prompt.entry());
        self.save_history();
        let text = self.prompt.take();
        self.prompt.set_shell(false);
        // On a subagent's tab the prompt is its; a command is the main
        // session's, so its output shows on the chat tab.
        if let Some(id) = self.showing_subagent() {
            match shell {
                true => self.open_content(super::Tab::Chat),
                false => return self.prompt_subagent(id, text),
            }
        }
        // Sent into the running turn at its next step, or held until it
        // ends; the chat shows it when the model gets it, so the
        // transcript keeps the order the model saw.
        let next = match shell {
            true => Queued::Shell(text),
            false => {
                self.hold_notices = false;
                Queued::Prompt(text)
            }
        };
        self.send(next);
    }

    /// Runs `next` as a turn, holds it until the running one ends, or, for
    /// a plain prompt while one runs, steers it: the turn picks it up from
    /// the model's inbox at its next step, pre-empting the tool calls it
    /// was about to run. A skill is filled in when its own turn starts,
    /// what already waits keeps its place ahead, and a prompt for another
    /// mode or model than the running turn's runs on its own, so those
    /// queue.
    pub(super) fn send(&mut self, next: Queued) {
        if let Queued::Prompt(text) = &next
            && self.is_busy()
            && self.queue.is_empty()
            && self.steerable == Some((self.mode, self.llm(self.mode)))
            && nth_context::skills::parse(text, &self.context.skills).is_none()
        {
            self.inbox.post_prompt(text.clone());
            return;
        }
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
            Queued::Approve => self.start_turn(self.approval()),
            Queued::PlanEdits { original, edited } => {
                self.start_turn(plan::edits::render(&self.plan_path, &original, &edited))
            }
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
        // A command never asks the model, so nothing would pick a prompt up.
        self.steerable = None;
        let events = self.events_tx.clone();
        let store = self.store.clone();
        let subagents = self.subagents.clone();
        self.turn.start(|token| {
            tokio::spawn(async move {
                let result = session
                    .shell(command, shell.as_ref(), &events, &token)
                    .await;
                session.usage.extend(subagents.take_spent());
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
        if self.session.is_none() || (text.is_empty() && !self.inbox.has_notices()) {
            return;
        }
        let Some(mut session) = self.session.take() else {
            return;
        };
        // `/name args` runs a skill: the chat shows it as typed, and the
        // model gets the skill filled in.
        let skill = nth_context::skills::parse(&text, &self.context.skills)
            .map(|(skill, args)| (skill.clone(), args.to_string()));
        // A skill is filled in before the model is asked, and that can
        // fail; notices taken now would be lost with the turn. They stay
        // in the inbox, where the loop hands them over between steps and
        // the end of the turn wakes the model for the rest.
        let notices = match skill {
            Some(_) => None,
            None => self.inbox.take_notices(),
        };
        // Prompts the last turn never picked up are this one's own: the
        // session treats them as it does `text`.
        let text = match skill {
            Some(_) => text,
            None => match self.inbox.take_prompts() {
                Some(prompts) if text.is_empty() => prompts,
                Some(prompts) => format!("{prompts}\n\n{text}"),
                None => text,
            },
        };
        self.notices_due = None;
        // Picked in the model picker since the last turn, maybe mid-turn.
        if session.model != self.model {
            session.set_model(self.model.clone());
        }
        // Added with `/add-dir`, maybe while the last turn ran: the session
        // is in its task while one does, so they reach it here.
        if session.extra_dirs != self.extra_dirs {
            session.set_extra_dirs(self.extra_dirs.clone());
        }
        self.llm_usage.count(&self.model);
        self.save_llm_usage();
        self.spent.turn_started(&self.model);
        session.effort = self.effort;
        session.mode = self.mode;
        self.steerable = Some((self.mode, self.llm(self.mode)));
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
        let subagents = self.subagents.clone();
        let front_end = FrontEnd {
            asker: Asker::new(self.asks_tx.clone()),
            screen: Screen::new(self.screen_tx.clone()),
            monitors: self.monitors.clone(),
            inbox: self.inbox.clone(),
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
                // Subagents that finished while it ran spent on its behalf.
                session.usage.extend(subagents.take_spent());
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
        self.drain_events();
        self.drop_asks();
        let elapsed = self
            .busy_since
            .take()
            .map_or(Duration::ZERO, |t| t.elapsed());
        let transcript = &mut self.chat.transcript;
        // Esc after the reply ended still comes back `Ok`, but it still
        // means stop.
        let interrupted = std::mem::take(&mut self.interrupted)
            || matches!(result, Err(nth_session::Error::Interrupted));
        let send_next = result.is_ok() && !interrupted;
        let error = match &result {
            Err(nth_session::Error::Interrupted) | Ok(()) => None,
            Err(e) => Some(e.to_string()),
        };
        self.last_turn = match &result {
            Ok(()) if send_next => TabState::Done,
            Ok(()) | Err(nth_session::Error::Interrupted) => TabState::Idle,
            Err(_) => TabState::Failed,
        };
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
        // `Moved` has normally been followed already; this catches a move
        // whose event never arrived.
        if session.cwd != self.cwd {
            self.cwd = session.cwd.clone();
            self.follow_place();
        }
        let plan_path = session.plan_path();
        self.session = Some(session);
        self.index_files();
        self.load_git();
        self.load_pr();
        // The plan lives under the working directory, so a move moves it.
        match plan_path != self.plan_path {
            true => self.plan_for_session(plan_path),
            false => self.read_plan(),
        }
        // Notices that came after the model's last step go with the next
        // prompt once you stopped it, else wake it on their own: a turn
        // that failed is no reason to keep a subagent's answer from it.
        self.hold_notices = interrupted;
        self.steerable = None;
        if !send_next {
            // What the turn never picked up goes back, steered or queued:
            // sent prompts were written for a turn that went well. The
            // notices stay and wake the model on their own.
            self.unqueue();
        } else if self.inbox.prompts_waiting().is_some() {
            // Steered prompts the turn never picked up were sent before
            // anything that queued behind them, so they run first.
            self.start_turn(String::new());
        } else {
            match self.queue.pop_front() {
                Some(next) => self.start(next),
                None => self.start_turn(String::new()),
            }
        }
        // What waited through a failed turn: no new notice will come to
        // arm the timer for it.
        if !self.is_busy() && !self.hold_notices && self.inbox.has_notices() {
            self.notices_due = Some(tokio::time::Instant::now() + NOTICE_DELAY);
        }
        self.notify_turn_ended(shell, error, elapsed);
    }

    /// A worktree tool moved the session: the place, git state and files
    /// shown follow it while the turn goes on.
    pub(super) fn follow(&mut self, cwd: PathBuf) {
        self.cwd = cwd;
        self.follow_place();
        self.load_git();
        self.load_pr();
        self.index_files();
    }

    fn follow_place(&mut self) {
        let home = std::env::var("HOME").ok();
        self.place = status::place(&self.cwd, home.as_deref());
    }

    /// Events sent just before the task returned may still be queued, and
    /// they belong above the footer. Through `on_session`, so a usage
    /// report at the tail counts too.
    fn drain_events(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            self.on_session(event);
        }
    }

    /// The turn task panicked, or was aborted, and the session it held is
    /// gone with it. The app goes on in a fresh session in the same
    /// directory rather than exit: the lost one was saved after its
    /// previous turn, so `/resume` brings it back to there. A failed turn
    /// otherwise: the queue goes back into the prompt.
    pub(super) fn turn_task_failed(&mut self, error: JoinError) {
        self.drain_events();
        self.drop_asks();
        let elapsed = self
            .busy_since
            .take()
            .map_or(Duration::ZERO, |t| t.elapsed());
        self.interrupted = false;
        let cx = self.notify_context(elapsed);
        self.notify(Event::Failed(format!("turn task failed: {error}")), &cx);
        self.chat.transcript.fail_turn(format!(
            "turn task failed: {error} · continuing in a new session; \
             the old one was last saved after its previous turn, /resume brings it back"
        ));
        self.steerable = None;
        // Before the fresh session: starting one clears the inbox, and a
        // steered prompt still in it is yours to give back.
        self.unqueue();
        self.start_fresh_session();
    }

    /// After an interrupted or failed turn, the queued prompts go back into
    /// the prompt ahead of what is typed: sent prompts were written for a
    /// turn that went well, so they wait to be looked at again. An
    /// approval or plan edits have no text of yours to give back, so they
    /// are dropped, and the status bar says so.
    fn unqueue(&mut self) {
        let mut parts = Vec::new();
        let mut dropped = Vec::new();
        // Prompts on their way into the turn were typed before what queued.
        if let Some(prompts) = self.inbox.take_prompts() {
            parts.push(prompts);
        }
        for next in self.queue.drain(..) {
            match next {
                Queued::Prompt(_) | Queued::Shell(_) => parts.push(next.label()),
                Queued::Approve => dropped.push("plan approval discarded"),
                Queued::PlanEdits { .. } => dropped.push("plan edits discarded"),
            }
        }
        dropped.dedup();
        if !dropped.is_empty() {
            self.hint = Some(dropped.join(" · "));
        }
        if parts.is_empty() {
            return;
        }
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
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use crossterm::event::{KeyCode, KeyEvent};
    use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
    use nth_protocol::{
        BoxError, Event, Listing, Message, MonitorEvent, Provider, Request, Stream, StreamEvent,
        ToolCall, Usage,
    };
    use tokio::sync::Notify;

    use super::*;
    use crate::chat::Entry;

    /// A provider that never answers: it says when a request reaches it,
    /// and records when that request is dropped.
    #[derive(Default)]
    struct Hang {
        asked: Arc<Notify>,
        dropped: Arc<AtomicBool>,
    }

    struct SetOnDrop(Arc<AtomicBool>);

    impl Drop for SetOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    impl Provider for Hang {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            async { Ok(Listing::default()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            // A permit, so a test that looks after the request arrived
            // still sees it.
            self.asked.notify_one();
            let guard = SetOnDrop(self.dropped.clone());
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
        let hang = Arc::new(Hang::default());
        let dropped = hang.dropped.clone();
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(session, hang, Arc::new(Vec::new()));
        app.prompt.insert_str("go");
        app.submit();
        tokio::task::yield_now().await;
        assert!(app.is_busy());
        (app, dropped)
    }

    /// Lets the running turn task finish before the app hears of it, as
    /// when the loop is busy drawing.
    async fn settle(app: &App) {
        let handle = app.turn.abort_handle().expect("running");
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }
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
        let hang = Arc::new(Hang::default());
        let session = Session::new("glm", dir.path().to_path_buf())
            .with_context(crate::app::tests::with_fix_skill(dir.path()));
        let mut app = App::new(session, hang.clone(), Arc::new(Vec::new()));

        app.prompt.insert_str("/fix the build");
        app.submit();
        // The skill is read first; Esc before the request would lose it.
        hang.asked.notified().await;
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
        let mut app = App::new(session, Arc::new(Hang::default()), Arc::new(Vec::new()));

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
        let mut app = App::new(session, Arc::new(Hang::default()), Arc::new(Vec::new()))
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
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            async { Ok(Listing::default()) }.boxed()
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

    /// Answers the first request with a tool call once `gate` opens, and
    /// every later one with text: a model mid-work when you steer it.
    struct Gated {
        gate: Arc<Notify>,
        requests: Arc<AtomicUsize>,
    }

    impl Provider for Gated {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            async { Ok(Listing::default()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            let (gate, requests) = (self.gate.clone(), self.requests.clone());
            async move {
                let events = match requests.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        gate.notified().await;
                        vec![StreamEvent::ToolCall(ToolCall {
                            id: "1".into(),
                            name: "bash".into(),
                            arguments: "{}".into(),
                        })]
                    }
                    _ => vec![StreamEvent::TextDelta("changed plans".into())],
                };
                Ok(futures::StreamExt::boxed(futures::stream::iter(
                    events.into_iter().map(Ok),
                )))
            }
            .boxed()
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
    async fn enter_while_busy_steers_the_prompt_into_the_turn() {
        let (mut app, _) = busy_app().await;

        send(&mut app, "next");

        assert!(app.prompt.is_empty());
        assert!(app.queue.is_empty());
        assert_eq!(app.inbox.prompts_waiting(), Some((1, "next".into())));
        assert_eq!(last_user(&app), Some("go"), "shown once the model gets it");
        let token = app.turn.token().expect("still running");
        assert!(!token.is_cancelled());
    }

    #[tokio::test]
    async fn prompts_sent_mid_turn_join_the_turn_that_picks_them_up() {
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(session, Arc::new(Answer), Arc::new(Vec::new()));
        send(&mut app, "one");
        send(&mut app, "two");
        send(&mut app, "three");

        end(&mut app).await;
        assert!(app.is_busy(), "the steered prompts' own turn");
        assert_eq!(last_user(&app), Some("two\n\nthree"));
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
        assert_eq!(sent, ["one", "two\n\nthree"]);
    }

    #[tokio::test]
    async fn a_prompt_sent_mid_turn_preempts_the_tool_calls() {
        let gate = Arc::new(Notify::new());
        let provider = Gated {
            gate: gate.clone(),
            requests: Arc::new(AtomicUsize::new(0)),
        };
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(session, Arc::new(provider), Arc::new(Vec::new()));
        send(&mut app, "go");
        send(&mut app, "stop, just name them");

        gate.notify_one();
        end(&mut app).await;

        assert!(!app.is_busy());
        assert!(app.queue.is_empty());
        let session = app.session.as_ref().expect("session came back");
        assert!(matches!(
            &session.messages[2],
            Message::Assistant(reply) if reply.tool_calls.len() == 1
        ));
        assert!(matches!(
            &session.messages[3],
            Message::ToolResult { content, .. }
                if content == "Error: interrupted by the user"
        ));
        let Message::User(steered) = &session.messages[4] else {
            panic!("the prompt was steered in: {:?}", session.messages[4]);
        };
        assert!(
            steered.starts_with("stop, just name them\n\n<system-reminder>\nThe user interrupted"),
            "{steered}"
        );
        assert!(matches!(
            &session.messages[5],
            Message::Assistant(reply) if reply.text == "changed plans"
        ));
        // The chat shows the call it skipped and what you typed, in the
        // order the model saw them.
        let entries: Vec<_> = app.chat.transcript.entries().collect();
        let at = entries
            .iter()
            .position(|entry| matches!(entry, Entry::Tool { call, .. } if call.name == "bash"))
            .expect("the skipped call shows");
        assert!(matches!(
            &entries[at],
            Entry::Tool { state: crate::chat::ToolState::Failed(why), .. }
                if why == "interrupted by the user"
        ));
        assert!(matches!(
            &entries[at + 1],
            Entry::User(text) if text == "stop, just name them"
        ));
    }

    #[tokio::test]
    async fn a_prompt_after_a_mode_switch_waits_for_its_own_turn() {
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(session, Arc::new(Answer), Arc::new(Vec::new()));
        send(&mut app, "go");
        app.set_mode(nth_protocol::Mode::Plan);
        send(&mut app, "plan it");

        assert_eq!(app.queue, [Queued::Prompt("plan it".into())]);
        assert_eq!(app.inbox.prompts_waiting(), None, "not steered");
        end(&mut app).await;
        assert_eq!(last_user(&app), Some("plan it"));
        end(&mut app).await;
        let session = app.session.as_ref().expect("session came back");
        assert_eq!(session.mode, nth_protocol::Mode::Plan);
    }

    #[tokio::test]
    async fn a_prompt_after_a_model_pick_waits_for_its_own_turn() {
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(session, Arc::new(Answer), Arc::new(Vec::new()));
        send(&mut app, "go");
        app.model = "kimi".into();
        send(&mut app, "you now");

        assert_eq!(app.queue, [Queued::Prompt("you now".into())]);
        end(&mut app).await;
        end(&mut app).await;
        let session = app.session.as_ref().expect("session came back");
        assert_eq!(session.model, "kimi");
    }

    #[tokio::test]
    async fn a_steered_prompt_runs_before_what_queued_behind_it() {
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(session, Arc::new(Answer), Arc::new(Vec::new()));
        send(&mut app, "go");
        send(&mut app, "next");
        command(&mut app, "make");
        assert_eq!(app.inbox.prompts_waiting(), Some((1, "next".into())));

        end(&mut app).await;
        assert!(app.is_busy(), "the steered prompt's own turn");
        assert_eq!(last_user(&app), Some("next"));
        assert_eq!(app.queue, [Queued::Shell("make".into())], "still waits");
    }

    #[tokio::test]
    async fn a_prompt_sent_while_a_command_runs_waits_for_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = shell_app(dir.path());
        command(&mut app, "echo hi");
        send(&mut app, "hello");

        assert_eq!(app.queue, [Queued::Prompt("hello".into())]);
        assert_eq!(
            app.inbox.prompts_waiting(),
            None,
            "nothing would pick it up"
        );
    }

    #[tokio::test]
    async fn the_status_bar_counts_prompts_on_their_way_into_the_turn() {
        let (mut app, _) = busy_app().await;
        send(&mut app, "next");

        let rows = crate::app::tests::rows(&mut app);
        assert!(rows[15].starts_with(" ⏵ 1 queued · next"), "{:?}", rows[15]);

        // A command waiting behind it keeps its place; the prompt on its
        // way in is still the next thing the model hears.
        command(&mut app, "make");
        let rows = crate::app::tests::rows(&mut app);
        assert!(rows[15].starts_with(" ⏵ 2 queued · next"), "{:?}", rows[15]);
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
    async fn esc_drops_queued_plan_edits_and_approval_with_a_hint() {
        let (mut app, _) = busy_app().await;
        app.plan_edited("# Plan\n", Ok("# Plan\nmore\n".into()));
        assert!(matches!(app.queue.front(), Some(Queued::PlanEdits { .. })));

        app.on_key(KeyEvent::from(KeyCode::Esc));
        end(&mut app).await;

        assert!(app.queue.is_empty());
        assert!(app.prompt.is_empty(), "{:?}", app.prompt.text());
        assert_eq!(app.hint.as_deref(), Some("plan edits discarded"));

        let (mut app, _) = busy_app().await;
        app.queue.push_back(Queued::Approve);
        app.queue.push_back(Queued::Prompt("then this".into()));
        app.on_key(KeyEvent::from(KeyCode::Esc));
        end(&mut app).await;

        assert_eq!(app.prompt.text(), "then this");
        assert_eq!(app.hint.as_deref(), Some("plan approval discarded"));
    }

    #[tokio::test]
    async fn a_usage_report_at_the_tail_of_a_turn_still_counts() {
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(session, Arc::new(Answer), Arc::new(Vec::new()));
        send(&mut app, "one");
        settle(&app).await;
        let usage = Usage {
            input: 10,
            output: 2,
            ..Usage::default()
        };
        app.events_tx.try_send(Event::Usage(usage)).expect("room");

        end(&mut app).await;

        assert_eq!(app.usage, Some(usage));
    }

    #[tokio::test]
    async fn a_turn_task_that_panics_leaves_the_app_running() {
        let (mut app, _) = busy_app().await;
        send(&mut app, "next");
        // In place of the real task, whose session is lost with it.
        app.turn
            .start(|_| tokio::spawn(async { panic!("tool bug") }));

        let Err(error) = app.turn.join().await else {
            panic!("the task panicked");
        };
        app.turn_task_failed(error);

        assert!(!app.is_busy());
        assert_eq!(app.prompt.text(), "next", "the queue comes back");
        let session = app.session.as_ref().expect("a fresh session");
        assert!(session.is_empty(), "nothing of the lost one");
        assert_eq!(session.cwd, std::path::Path::new("/repo"));
        let Some(Entry::TurnError(e)) = app.chat.transcript.entries().last() else {
            panic!(
                "the turn failed: {:?}",
                app.chat.transcript.entries().last()
            );
        };
        assert!(e.starts_with("turn task failed: "), "{e}");
        assert!(e.contains("/resume"), "{e}");
    }

    #[tokio::test]
    async fn esc_after_the_reply_ended_still_holds_the_queue_back() {
        let session = Session::new("glm", "/repo".into());
        let mut app = App::new(session, Arc::new(Answer), Arc::new(Vec::new()));
        send(&mut app, "one");
        send(&mut app, "two");
        settle(&app).await;

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
            Some(Entry::Notice(nth_protocol::NoticeSummary::Monitor {
                lines: 1,
                ..
            }))
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
    async fn after_a_failed_turn_a_notice_wakes_the_model() {
        let mut app = app_logging_to_tmp(Arc::new(crate::app::tests::Idle));
        send(&mut app, "go");
        end(&mut app).await;
        assert!(matches!(
            app.chat.transcript.entries().last(),
            Some(Entry::TurnError(_))
        ));

        monitor_says(&mut app, "build failed").await;
        app.notices_due();

        assert!(app.is_busy(), "a failure is not a stop");
        end(&mut app).await;
        // The failed "go" was never answered, so the notice joins it.
        assert_eq!(sent(&app), [format!("go\n\n{NOTICE}")]);
    }

    #[tokio::test]
    async fn a_notice_during_a_failed_turn_wakes_the_model_after_it() {
        let mut app = app_logging_to_tmp(Arc::new(crate::app::tests::Idle));
        send(&mut app, "go");
        monitor_says(&mut app, "build failed").await;
        app.notices_due();
        end(&mut app).await;

        assert!(app.notices_due.is_some(), "nothing else will arm it");
        app.notices_due();
        assert!(app.is_busy());
        end(&mut app).await;
        assert_eq!(sent(&app), [format!("go\n\n{NOTICE}")]);
    }

    #[tokio::test]
    async fn a_skill_that_cannot_be_read_keeps_the_notices() {
        let dir = tempfile::tempdir().expect("tempdir");
        let context = crate::app::tests::with_fix_skill(dir.path());
        std::fs::remove_file(dir.path().join(".agents/skills/fix/SKILL.md")).expect("removes");
        let session = Session::new("glm", dir.path().to_path_buf()).with_context(context);
        let mut app = App::new(session, Arc::new(Hang::default()), Arc::new(Vec::new()));
        app.monitors.set_log_dir("/tmp/logs".into());
        monitor_says(&mut app, "build failed").await;

        send(&mut app, "/fix it");
        end(&mut app).await;

        assert!(matches!(
            app.chat.transcript.entries().last(),
            Some(Entry::TurnError(_))
        ));
        assert!(
            app.inbox.has_notices(),
            "not taken by a turn that never ran"
        );
        assert!(app.notices_due.is_some(), "and still wake the model");
        assert_eq!(sent(&app), Vec::<&str>::new());
    }

    #[tokio::test]
    async fn clear_stops_the_monitors() {
        let mut app = app_logging_to_tmp(Arc::new(Answer));
        monitor_says(&mut app, "build failed").await;

        app.run_command(Command::Clear);

        assert_eq!(app.monitors.running(), 0);
        assert!(!app.inbox.has_notices());
    }

    #[tokio::test]
    async fn closing_the_picker_leaves_the_turn_running() {
        let (mut app, _) = busy_app().await;
        app.llms = Some(Listing::default());
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

    #[tokio::test]
    async fn a_turn_that_moved_the_session_moves_the_app() {
        let mut app = crate::app::tests::app();
        let mut session = app.session.take().expect("idle");
        let worktree = app.cwd.join(".nth/worktrees/x");
        session.set_cwd(worktree.clone(), Some(app.cwd.clone()));

        app.end_turn(Ended {
            session,
            result: Ok(()),
            saved: Ok(()),
            shell: false,
        });

        assert_eq!(app.cwd, worktree);
        assert!(app.place.ends_with(".nth/worktrees/x"), "{}", app.place);
        let plan = app.session.as_ref().expect("back").plan_path();
        assert!(plan.starts_with(&worktree), "{}", plan.display());
        assert_eq!(app.plan_path, plan);
    }

    #[tokio::test]
    async fn the_status_bar_follows_a_move_while_the_turn_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical");
        let worktree = root.join(".nth/worktrees/x");
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .expect("git");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&["init", "-q", "-b", "main"]);
        git(&[
            "-c",
            "user.name=nth",
            "-c",
            "user.email=nth@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ]);
        git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "x",
            &worktree.to_string_lossy(),
        ]);
        let mut app = crate::app::tests::app();
        // Mid-turn the session is in the turn's task.
        app.session = None;

        app.on_session(Event::Moved(worktree.clone()));
        let status = app.git_loading.join().await.expect("loads");
        app.git_loaded(status);

        assert_eq!(app.cwd, worktree);
        assert!(app.place.ends_with(".nth/worktrees/x"), "{}", app.place);
        // What the status bar's git summary shows.
        let branch = app.git.as_ref().and_then(|git| git.branch.as_deref());
        assert_eq!(branch, Some("x"));
    }
}
