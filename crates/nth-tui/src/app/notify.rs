//! Desktop notifications for when you look elsewhere: the model done or
//! failed, a plan ready to approve, a question waiting.

use std::time::Duration;

use nth_notify::{Context, Event, Notifier};
use nth_protocol::{Ask, Message, Mode};

use super::{App, TabState};

impl App {
    pub fn with_notifier(mut self, notifier: Notifier) -> Self {
        self.notify_errors = notifier.errors();
        self.notifier = notifier;
        self
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
            // Whether there is a plan is known once the read `end_turn`
            // started is back; `plan_read` sends it then.
            None if self.last_turn == TabState::Done && self.mode == Mode::Plan => {
                self.plan_notice = Some(cx)
            }
            None if self.last_turn == TabState::Done => self.notify(Event::Done, &cx),
            // Stopped with Esc: you are here.
            None => {}
        }
    }

    /// After the plan is read for a turn that ended in plan mode.
    pub(super) fn notify_plan_read(&mut self) {
        if let Some(cx) = self.plan_notice.take() {
            let event = match self.plan.exists() {
                true => Event::PlanReady,
                false => Event::Done,
            };
            self.notify(event, &cx);
        }
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

    use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
    use nth_notify::{Backend, BoxError as NotifyError, Notification, Urgency};
    use nth_protocol::{BoxError, ModelInfo, Provider, Question, Request, StreamEvent};
    use nth_session::Session;
    use tokio::sync::{mpsc, oneshot};

    use super::*;

    /// Answers every request with `reply`, or fails with it.
    struct Reply {
        text: &'static str,
        fail: bool,
    }

    impl Provider for Reply {
        fn models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, BoxError>> {
            async { Ok(Vec::new()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            let (text, fail) = (self.text, self.fail);
            async move {
                if fail {
                    return Err(text.into());
                }
                let reply = futures::stream::iter([Ok(StreamEvent::TextDelta(text.into()))]);
                Ok(futures::StreamExt::boxed(reply))
            }
            .boxed()
        }
    }

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
        provider: Reply,
    ) -> (App, mpsc::UnboundedReceiver<Notification>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let session = Session::new("glm", cwd);
        let app = App::new(session, Arc::new(provider), Arc::new(Vec::new()))
            .with_notifier(Notifier::new(Box::new(Recording(tx))));
        (app, rx)
    }

    fn answering(text: &'static str) -> (App, mpsc::UnboundedReceiver<Notification>) {
        app_in("/repo".into(), Reply { text, fail: false })
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
        assert_eq!(n.urgency, Urgency::Normal);
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
    async fn a_failed_turn_is_critical_with_the_error() {
        let (mut app, mut rx) = app_in(
            "/repo".into(),
            Reply {
                text: "rate limited",
                fail: true,
            },
        );
        send(&mut app, "go");
        end(&mut app).await;

        let n = next(&mut rx).await;
        assert!(n.summary.starts_with("nth · failed"), "{}", n.summary);
        assert!(n.body.contains("rate limited"), "{}", n.body);
        assert_eq!(n.urgency, Urgency::Critical);
    }

    #[tokio::test]
    async fn a_plan_turn_waits_for_the_plan_to_say_it_is_ready() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut app, mut rx) = app_in(
            dir.path().to_path_buf(),
            Reply {
                text: "planned",
                fail: false,
            },
        );
        app.set_mode(Mode::Plan);
        std::fs::create_dir_all(app.plan_path.parent().expect("dir")).expect("dirs");
        std::fs::write(&app.plan_path, "# Plan\n").expect("writes");
        send(&mut app, "plan it");
        end(&mut app).await;
        none(&mut rx).await;

        while app.plan_reading.is_running() {
            let text = app.plan_reading.join().await.expect("read");
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
        assert_eq!(n.urgency, Urgency::Critical);
    }
}
