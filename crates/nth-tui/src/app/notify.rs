//! Desktop notifications for when you look elsewhere: the model done or
//! failed, a plan ready to approve, a question waiting. Only while the
//! terminal does not have focus, so a terminal that never reports focus
//! never notifies.

use std::time::Duration;

use nth_notify::{Context, Event, Notifier};
use nth_protocol::{Ask, Message, Mode};

use super::{App, TabState};

impl App {
    /// Sends through `notifier` from now on, and hears of its failures
    /// rather than the last one's.
    pub(super) fn set_notifier(&mut self, notifier: Notifier) {
        self.notify_errors = notifier.errors();
        self.notifier = notifier;
    }

    /// At the end of a turn, once it is clear whether nth now waits on you:
    /// a queued prompt or waiting notices may have started the next turn.
    /// `error` is why the turn failed, if it did.
    pub(super) fn notify_turn_ended(
        &mut self,
        shell: bool,
        error: Option<String>,
        elapsed: Duration,
    ) {
        if shell || self.is_busy() {
            return;
        }
        let cx = self.notify_context(elapsed);
        match error {
            Some(error) => self.notify(Event::Failed(error), &cx),
            None if self.background_waiting() => {}
            // Whether there is a plan is known once the read `end_turn`
            // started is back; `plan_read` sends it then.
            None if self.last_turn == TabState::Done && self.selection.mode == Mode::Plan => {
                self.plan.notice = Some(cx)
            }
            None if self.last_turn == TabState::Done => self.notify(Event::Done, &cx),
            // Stopped with Esc: you are here.
            None => {}
        }
    }

    /// After the plan is read for a turn that ended in plan mode.
    pub(super) fn notify_plan_read(&mut self) {
        let notice = self.plan.notice.take();
        if self.is_busy() || self.background_waiting() {
            return;
        }
        if let Some(cx) = notice {
            let event = match self.plan.view.exists() {
                true => Event::PlanReady,
                false => Event::Done,
            };
            self.notify(event, &cx);
        }
    }

    /// A monitor's end the app has not heard yet counts too: it reaches the
    /// inbox, and wakes the model, only once heard.
    fn background_waiting(&self) -> bool {
        self.subagents.running() > 0
            || self.monitors.running() > 0
            || self.awaits_monitor_ends()
            || self.inbox.wakes()
    }

    /// For every ask as it arrives, including those queued behind the one
    /// showing.
    pub(super) fn notify_ask(&self, ask: &Ask) {
        let Some((first, rest)) = ask.questions.split_first() else {
            return;
        };
        let elapsed = self.busy_since.map_or(Duration::ZERO, |t| t.elapsed());
        let event = Event::NeedsYou {
            header: first.header.clone(),
            question: first.question.clone(),
            more: rest.len(),
        };
        self.notify(event, &self.notify_context(elapsed));
    }

    /// The backend failed: said once, so notifications that never arrive
    /// are not a mystery.
    pub(super) fn notify_failed(&mut self) {
        if let Some(error) = self.notify_errors.borrow_and_update().clone() {
            self.chat
                .transcript
                .push_error(format!("notification not shown: {error}"));
        }
    }

    pub(super) fn notify_context(&self, elapsed: Duration) -> Context {
        let session = self.session.as_ref();
        Context {
            title: session.and_then(|s| s.title()).unwrap_or_default(),
            elapsed,
            reply: session.and_then(|s| last_reply(&s.messages)),
        }
    }

    pub(super) fn notify(&self, event: Event, cx: &Context) {
        if self.focused {
            return;
        }
        self.notifier.notify(event.notification(cx));
    }
}

/// The model's last words with text: a turn often ends on its answer, but
/// the step before a tool call may be the only text there is.
fn last_reply(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| match message {
        Message::Assistant(reply) if !reply.text.trim().is_empty() => Some(reply.text.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use crossterm::event::Event as TermEvent;
    use futures::{FutureExt, future::BoxFuture};
    use nth_notify::{Backend, BoxError as NotifyError, Notification};
    use nth_protocol::{MonitorEnd, MonitorEvent, Question};
    use nth_session::Session;
    use tokio::sync::{mpsc, oneshot};

    use super::*;
    use crate::app::tests::{Reply, Scripted, app_on};

    struct Recording(mpsc::UnboundedSender<Notification>);

    impl Backend for Recording {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn send<'a>(&'a self, n: &'a Notification) -> BoxFuture<'a, Result<(), NotifyError>> {
            let _ = self.0.send(n.clone());
            async { Ok(()) }.boxed()
        }
    }

    fn app_in(
        cwd: std::path::PathBuf,
        provider: Scripted,
    ) -> (App, mpsc::UnboundedReceiver<Notification>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let session = Session::new("glm", cwd);
        let mut app = app_on(session, Arc::new(provider));
        // A backend no config names: the one that records what it was sent.
        app.set_notifier(Notifier::new(Box::new(Recording(tx))));
        // Looking elsewhere, or nothing would be sent.
        app.on_terminal(TermEvent::FocusLost);
        (app, rx)
    }

    fn answering(text: &'static str) -> (App, mpsc::UnboundedReceiver<Notification>) {
        app_in(
            "/repo".into(),
            Scripted::default().with_fallback(Reply::text(text)),
        )
    }

    fn send(app: &mut App, text: &str) {
        app.prompt.insert_str(text);
        app.submit();
    }

    async fn end(app: &mut App) {
        let ended = app.turn.join().await.expect("turn task finished");
        app.end_turn(ended);
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<Notification>) -> Notification {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("in time")
            .expect("sent")
    }

    /// Nothing sent, once the notifier's task had every chance to.
    async fn none(rx: &mut mpsc::UnboundedReceiver<Notification>) {
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert_eq!(rx.try_recv().ok(), None);
    }

    #[tokio::test]
    async fn a_finished_turn_says_where_and_what_the_model_said() {
        let (mut app, mut rx) = answering("all done");
        send(&mut app, "go");
        end(&mut app).await;

        let n = next(&mut rx).await;
        assert_eq!(n.summary, "nth · done in 0s");
        assert_eq!(n.body, "go\nall done");
    }

    #[tokio::test]
    async fn only_the_last_of_queued_turns_notifies() {
        let (mut app, mut rx) = answering("ok");
        send(&mut app, "one");
        send(&mut app, "two");
        end(&mut app).await;
        assert!(app.is_busy());
        none(&mut rx).await;

        end(&mut app).await;
        assert!(next(&mut rx).await.summary.contains("done"));
        none(&mut rx).await;
    }

    #[tokio::test]
    async fn esc_sends_nothing() {
        let (mut app, mut rx) = answering("ok");
        send(&mut app, "go");
        app.interrupt();
        end(&mut app).await;
        none(&mut rx).await;
    }

    #[tokio::test]
    async fn a_focused_terminal_sends_nothing() {
        let (mut app, mut rx) = answering("ok");
        app.on_terminal(TermEvent::FocusGained);
        send(&mut app, "go");
        end(&mut app).await;
        none(&mut rx).await;
    }

    #[tokio::test]
    async fn a_failed_turn_names_the_error() {
        let (mut app, mut rx) = app_in(
            "/repo".into(),
            Scripted::default().with_fallback(Reply::Fail("rate limited".into())),
        );
        send(&mut app, "go");
        end(&mut app).await;

        let n = next(&mut rx).await;
        assert!(n.summary.starts_with("nth · failed"), "{}", n.summary);
        assert!(n.body.contains("rate limited"), "{}", n.body);
    }

    #[tokio::test]
    async fn a_plan_turn_waits_for_the_plan_to_say_it_is_ready() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, mut rx) = app_in(
            dir.path().to_path_buf(),
            Scripted::default().with_fallback(Reply::text("planned")),
        );
        app.set_mode(Mode::Plan);
        std::fs::create_dir_all(app.plan.path().parent().expect("dir")).expect("dirs");
        std::fs::write(app.plan.path(), "# Plan\n").expect("writes");
        send(&mut app, "plan it");
        end(&mut app).await;
        none(&mut rx).await;

        while app.plan.reading.is_running() {
            let text = app.plan.reading.join().await.expect("read");
            app.plan_read(text);
        }
        let n = next(&mut rx).await;
        assert!(n.summary.ends_with("plan ready"), "{}", n.summary);
        assert!(n.body.contains("/approve"), "{}", n.body);
    }

    #[tokio::test]
    async fn a_question_says_what_it_asks() {
        let (app, mut rx) = answering("ok");
        let question = |header: &str, question: &str| Question {
            question: question.into(),
            header: header.into(),
            multiple: false,
            options: Vec::new(),
        };
        let (reply, _) = oneshot::channel();
        app.notify_ask(&Ask {
            call_id: "call".into(),
            questions: vec![question("Doom loop", "Keep going?"), question("x", "y")],
            reply,
        });

        let n = next(&mut rx).await;
        assert_eq!(n.summary, "nth · needs you");
        assert_eq!(n.body, "Doom loop: Keep going?\nand 1 more question");
    }

    #[tokio::test]
    async fn a_monitor_end_not_heard_yet_suppresses_done() {
        let (mut app, mut rx) = answering("ok");
        let m = crate::app::monitor::tests::start(&mut app, "build").await;
        send(&mut app, "go");
        let ended = MonitorEvent::Ended {
            id: m.id,
            end: MonitorEnd::Exited(Some(0)),
            events: 0,
        };
        app.monitors.event(ended).await;
        end(&mut app).await;
        none(&mut rx).await;
    }

    #[tokio::test]
    async fn running_monitor_suppresses_done_but_not_failure_or_question() {
        let (mut app, mut rx) = answering("ok");
        app.monitors
            .register("build", "watch")
            .await
            .expect("monitor");
        send(&mut app, "go");
        end(&mut app).await;
        none(&mut rx).await;

        app.notify_turn_ended(false, Some("failed".into()), Duration::ZERO);
        assert!(next(&mut rx).await.summary.contains("failed"));
        let (reply, _) = oneshot::channel();
        app.notify_ask(&Ask {
            call_id: "ask".into(),
            questions: vec![Question {
                header: "Help".into(),
                question: "Continue?".into(),
                multiple: false,
                options: vec![],
            }],
            reply,
        });
        assert!(next(&mut rx).await.summary.contains("needs you"));
        app.monitors.forget_all();
        send(&mut app, "again");
        end(&mut app).await;
        assert!(next(&mut rx).await.summary.contains("done"));
    }

    #[tokio::test]
    async fn running_subagent_without_a_view_suppresses_done() {
        use nth_session::{
            CancellationToken,
            subagent::{Done, Job},
        };

        let (mut app, mut rx) = answering("ok");
        let agents = nth_context::Context::discover(
            std::path::Path::new("/nowhere"),
            &nth_context::Paths::default(),
        )
        .agents;
        let id = app.subagents.spawn(
            app.subagents.reserve(),
            agents.get("explore").expect("agent"),
            "investigate",
            Session::new("glm", "/repo".into()).as_subagent(None),
            Arc::new(Scripted::hang()),
            Vec::new(),
        );
        assert!(app.subagents.prompt(
            id,
            Job {
                text: "go".into(),
                cancel: CancellationToken::new(),
                done: Done::Notify(app.inbox.clone()),
                timeout: None,
                stall: None,
            }
        ));
        tokio::time::timeout(Duration::from_secs(5), async {
            while app.subagents.running() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("subagent started");
        assert!(app.subagent_views.is_empty());
        send(&mut app, "go");
        end(&mut app).await;
        none(&mut rx).await;
        app.subagents.forget_all();
    }

    #[tokio::test]
    async fn delayed_plan_notice_is_discarded_if_background_work_starts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, mut rx) = app_in(
            dir.path().into(),
            Scripted::default().with_fallback(Reply::text("planned")),
        );
        app.set_mode(Mode::Plan);
        std::fs::create_dir_all(app.plan.path().parent().unwrap()).unwrap();
        std::fs::write(app.plan.path(), "# Plan\n").unwrap();
        send(&mut app, "plan");
        end(&mut app).await;
        assert!(app.plan.notice.is_some());
        app.monitors
            .register("build", "watch")
            .await
            .expect("monitor");
        while app.plan.reading.is_running() {
            let text = app.plan.reading.join().await.expect("read");
            app.plan_read(text);
        }
        assert!(app.plan.notice.is_none());
        none(&mut rx).await;
    }

    #[tokio::test]
    async fn wakeable_notice_suppresses_success_but_todo_does_not() {
        use nth_session::{
            subagent::{TaskNotice, TaskOutcome},
            work::{WorkNotice, WorkState},
        };
        let (mut app, mut rx) = answering("ok");
        app.last_turn = TabState::Done;
        app.inbox.post_task(TaskNotice {
            id: 1,
            name: "Atlas".into(),
            agent: "explore".into(),
            description: "search".into(),
            outcome: TaskOutcome::Completed("found".into()),
        });
        app.notify_turn_ended(false, None, Duration::ZERO);
        none(&mut rx).await;
        app.inbox.take_notices();
        app.inbox.post_work(WorkNotice {
            name: "Atlas".into(),
            cycle: "01".into(),
            item: "task".into(),
            task: 1,
            agent: "explore".into(),
            state: WorkState::Todo(vec![]),
        });
        assert!(app.inbox.has_notices());
        assert!(!app.inbox.wakes());
        app.notify_turn_ended(false, None, Duration::ZERO);
        assert!(next(&mut rx).await.summary.contains("done"));
    }

    #[tokio::test]
    async fn only_a_changed_notify_config_builds_the_notifier_again() {
        use nth_notify::BackendKind;

        let (mut app, mut rx) = answering("unused");
        let config = || {
            crate::app::tests::runtime(
                Session::new("glm", "/repo".into()),
                Arc::new(Scripted::idle()),
            )
            .config
        };
        // The app was built on this config, so the recording notifier set
        // after it stays while [notify] says the same.
        let note = |summary: &str| Notification {
            summary: summary.into(),
            body: String::new(),
        };

        app.apply_config(config());
        app.notifier.notify(note("kept"));
        assert_eq!(rx.recv().await, Some(note("kept")), "the same [notify]");

        let mut changed = config();
        changed.notify.backend = match BackendKind::default() {
            BackendKind::NotifySend => BackendKind::Osascript,
            BackendKind::Osascript => BackendKind::NotifySend,
        };
        app.apply_config(changed);
        // The recording notifier's task ends once its last handle is gone,
        // dropping the backend and so the channel's sender.
        assert_eq!(rx.recv().await, None, "a changed [notify] replaced it");
    }
}
