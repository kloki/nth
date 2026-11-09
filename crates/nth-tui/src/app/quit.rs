//! Quitting: asked twice while something runs, which is stopped on the
//! way out: a turn so the session keeps it, monitors and subagents so a
//! resumed session knows they are gone.

use std::time::Duration;

use nth_protocol::{Message, StoppedBy};

use super::App;
use crate::job::Job;

/// How long quitting waits for the stopped monitors to report back.
const EXIT_WAIT: Duration = Duration::from_secs(2);

/// How long quitting waits for a cancelled turn to end and save itself.
const TURN_EXIT_WAIT: Duration = Duration::from_secs(5);

/// How long quitting waits for the prompt history to be written.
const HISTORY_EXIT_WAIT: Duration = Duration::from_secs(1);

/// What a turn lost on the way out leaves you with.
const LAST_SAVED: &str = "the session was last saved after its previous turn";

impl App {
    /// Quits, but with a turn, monitors or subagents running only on the
    /// second ask in a row.
    pub(super) fn ask_quit(&mut self) {
        let turn = self.is_busy();
        let monitors = self.monitors.running();
        let subagents = self.running_subagents();
        if (!turn && monitors + subagents == 0) || self.quit_armed {
            self.quit = true;
            return;
        }
        self.quit_armed = true;
        let mut running = Vec::new();
        if turn {
            running.push("turn".to_string());
        }
        if monitors > 0 {
            let noun = if monitors == 1 { "monitor" } else { "monitors" };
            running.push(format!("{monitors} {noun}"));
        }
        if subagents > 0 {
            let noun = if subagents == 1 {
                "subagent"
            } else {
                "subagents"
            };
            running.push(format!("{subagents} {noun}"));
        }
        self.hint = Some(format!("{} running · ctrl+c again to quit", running.join(" · ")).into());
    }

    /// Stops the turn, every monitor and every subagent as nth quits, and
    /// saves the session with how they ended, so the model knows they are
    /// gone when it is resumed.
    pub(super) async fn stop_background_for_exit(&mut self) {
        self.quitting = true;
        // A turn whose own save failed hands back a session the file does
        // not hold yet.
        let mut unsaved = self.stop_turn_for_exit().await;
        self.curios.invalidate();
        self.stop_subagents_for_exit().await;
        self.monitors.stop_all(StoppedBy::Exit);
        // Each end reaches the inbox as the app hears it, so the save below
        // says how they ended, also of one that ended before you quit.
        let ended = async {
            while self.monitors.running() > 0 || self.awaits_monitor_ends() {
                let Some(event) = self.monitor_rx.recv().await else {
                    break;
                };
                self.on_monitor(event);
            }
        };
        let _ = tokio::time::timeout(EXIT_WAIT, ended).await;
        self.save_history_for_exit().await;
        // A save between turns is of an older clone of the session: landing
        // after the one below, it would roll the file back. One that failed
        // or panicked, or one asked for while it ran, left the file behind
        // the session, so the save below stands in for it.
        if self.session_saving.is_running() {
            let saved = self.session_saving.join().await;
            unsaved |= !matches!(saved, Ok(Ok(()))) | self.session_saving.take_again();
        }
        // Gone with a turn that would not stop, or whose task panicked.
        let Some(session) = &mut self.session else {
            return;
        };
        let spent = self.subagents.take_spent();
        let notices = self.inbox.take_notices();
        if spent.is_empty() && notices.is_none() && !unsaved {
            return;
        }
        session.usage.extend(spent);
        session.messages.extend(notices.map(Message::User));
        if let Some(store) = &self.store
            && let Err(e) = store.save(session).await
        {
            self.exit_notes.push(format!("session not saved: {e}"));
        }
    }

    /// Waits a moment for the prompt history to be written, so a prompt
    /// sent just before quitting, or dropped by it unsent, can still be
    /// recalled next time.
    async fn save_history_for_exit(&mut self) {
        let saved = async {
            while self.history_saving.is_running() {
                let saved = self.history_saving.join().await;
                if let Ok(Err(e)) = saved {
                    self.exit_notes
                        .push(format!("prompt history not saved: {e}"));
                }
                if self.history_saving.take_again() {
                    self.save_history();
                }
            }
        };
        let _ = tokio::time::timeout(HISTORY_EXIT_WAIT, saved).await;
    }

    /// Cancels a running turn and waits for it to save the session and
    /// hand it back, every call it was running answered as interrupted;
    /// true when that save failed. A turn that will not stop in time is
    /// aborted, and what it did is lost with it.
    async fn stop_turn_for_exit(&mut self) -> bool {
        if !self.turn.is_running() {
            return false;
        }
        self.turn.cancel();
        match tokio::time::timeout(TURN_EXIT_WAIT, self.turn.join()).await {
            Ok(Ok(ended)) => {
                let unsaved = ended.save_error().is_some();
                self.end_turn(ended);
                unsaved
            }
            Ok(Err(e)) => {
                self.exit_notes
                    .push(format!("the turn task failed: {e}; {LAST_SAVED}"));
                false
            }
            Err(_) => {
                // Aborted now rather than when the app is dropped, so it
                // runs no tool while the rest of the exit goes on.
                self.turn = Job::default();
                self.exit_notes.push(format!(
                    "the turn did not stop within {} s; {LAST_SAVED}",
                    TURN_EXIT_WAIT.as_secs()
                ));
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{future::Future, sync::Arc};

    use futures::future::BoxFuture;
    use nth_protocol::{
        INTERRUPTED, MonitorEnd, MonitorEvent, StreamEvent, Tool, ToolCall, ToolContext,
        ToolResult, ToolSpec,
    };
    use nth_session::{Session, Store};
    use tokio::sync::Notify;

    use super::*;
    use crate::{
        app::{
            AppConfig, Queued,
            keys::Action,
            monitor::tests::start,
            tests::{Reply, Scripted, app_on, test_app},
        },
        history::History,
    };

    /// An app whose turn waits on a model that never answers.
    fn busy_app() -> App {
        let session = Session::new("glm", "/repo".into());
        let mut app = app_on(session, Arc::new(Scripted::hang()));
        app.prompt.insert_str("go");
        app.submit();
        assert!(app.is_busy());
        app
    }

    /// A tool by this name that never returns, and says when it was
    /// called.
    struct Stuck(&'static str, Arc<Notify>);

    impl Tool for Stuck {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0,
                description: String::new(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }

        fn call<'a>(
            &'a self,
            _: serde_json::Value,
            _: &'a ToolContext,
        ) -> BoxFuture<'a, ToolResult> {
            self.1.notify_one();
            Box::pin(std::future::pending())
        }
    }

    /// The jobs the end of a turn starts, by name, that still run.
    fn running(app: &App) -> Vec<&'static str> {
        [
            ("turn", app.turn.is_running()),
            ("session save", app.session_saving.is_running()),
            ("plan read", app.plan.reading.is_running()),
            ("work read", app.work.reading.is_running()),
            ("file index", app.indexing.is_running()),
            ("git status", app.git.loading.is_running()),
            ("pull request", app.git.pr_loading.is_running()),
        ]
        .into_iter()
        .filter_map(|(job, running)| running.then_some(job))
        .collect()
    }

    #[tokio::test]
    async fn quitting_with_a_turn_running_asks_twice() {
        let mut app = busy_app();

        app.apply(Action::ClearOrQuit);
        assert!(!app.quit);
        let hint = app.hint.as_deref().unwrap_or_default();
        assert!(hint.starts_with("turn running · ctrl+c again"), "{hint}");
        app.apply(Action::ClearOrQuit);
        assert!(app.quit);
    }

    #[tokio::test]
    async fn exit_with_a_turn_running_asks_for_ctrl_c() {
        let mut app = busy_app();

        app.prompt.insert_str("/exit");
        app.submit();
        assert!(!app.quit);
        let hint = app.hint.as_deref().unwrap_or_default();
        assert!(hint.starts_with("turn running · ctrl+c again"), "{hint}");
        app.apply(Action::ClearOrQuit);
        assert!(app.quit);
    }

    #[tokio::test]
    async fn quitting_mid_turn_stops_the_turn_and_keeps_the_session() {
        // `bash` as well: its interrupted call would reload git status.
        for name in ["stuck", "bash"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = Store::at(dir.path());
            let session = Session::new("glm", "/repo".into());
            let id = session.id;
            let call = ToolCall {
                id: "1".into(),
                name: name.into(),
                arguments: "{}".into(),
            };
            let provider = Scripted::replying([Reply::Events(vec![StreamEvent::ToolCall(call)])])
                .with_fallback(Reply::Hang);
            let called = Arc::new(Notify::new());
            let tools: Vec<Arc<dyn Tool>> = vec![Arc::new(Stuck(name, called.clone()))];
            let mut config = AppConfig::test(session, Arc::new(provider));
            config.runtime.model_tools = tools;
            config.store = Some(store.clone());
            let mut app = App::new(config);
            app.prompt.insert_str("go");
            app.submit();
            called.notified().await;

            app.stop_background_for_exit().await;

            assert_eq!(running(&app), Vec::<&str>::new(), "{name}: left running");
            assert!(app.exit_notes.is_empty(), "{:?}", app.exit_notes);
            assert!(app.session.is_some(), "the session came back");
            let saved = store.load(id).await.expect("saved");
            assert!(saved.messages.contains(&Message::User("go".into())));
            assert_eq!(
                saved.messages.last(),
                Some(&Message::tool_failed("1", INTERRUPTED))
            );
        }
    }

    #[tokio::test]
    async fn quitting_starts_nothing_queued_behind_the_turn() {
        let session = Session::new("glm", "/repo".into());
        let mut app = app_on(session, Arc::new(Scripted::default()));
        app.prompt.insert_str("go");
        app.submit();
        app.turns.push(Queued::Prompt("then".into()));
        // The reply ended before the cancel came: the turn ends well, which
        // would start what queued behind it.
        let handle = app.turn.abort_handle().expect("running");
        while !handle.is_finished() {
            tokio::task::yield_now().await;
        }

        app.stop_background_for_exit().await;

        assert_eq!(running(&app), Vec::<&str>::new(), "left running");
        // Dropped with the app; the prompt history has it.
        assert_eq!(*app.turns.queued(), [Queued::Prompt("then".into())]);
    }

    /// An app with something to save, holding a save between turns that
    /// ends as `saved` does.
    fn saving(
        store: &Store,
        saved: impl Future<Output = Result<(), nth_session::store::Error>> + Send + 'static,
    ) -> App {
        saving_with(History::default(), store, saved)
    }

    /// [`saving`], recalling `history`.
    fn saving_with(
        history: History,
        store: &Store,
        saved: impl Future<Output = Result<(), nth_session::store::Error>> + Send + 'static,
    ) -> App {
        let mut session = Session::new("glm", "/repo".into());
        session.messages.push(Message::User("hi".into()));
        let mut app = App::new(AppConfig {
            store: Some(store.clone()),
            history,
            ..AppConfig::test(session, Arc::new(Scripted::hang()))
        });
        // An older clone's save, still writing as you quit; it writes
        // nothing, so the file holds only what the exit saves.
        app.session_saving.start(|_| {
            tokio::spawn(async move {
                tokio::task::yield_now().await;
                saved.await
            })
        });
        app
    }

    #[tokio::test]
    async fn quitting_saves_after_a_save_in_flight_that_left_the_file_behind() {
        for case in ["asked again", "failed", "panicked"] {
            let dir = tempfile::tempdir().expect("tempdir");
            let store = Store::at(dir.path());
            let mut app = match case {
                "asked again" => {
                    let mut app = saving(&store, async { Ok(()) });
                    app.save_session();
                    app
                }
                "failed" => saving(&store, async { Err(nth_session::store::Error::NoDataDir) }),
                _ => saving(&store, async { panic!("save exploded") }),
            };
            let id = app.session.as_ref().expect("idle").id;

            app.stop_background_for_exit().await;

            assert_eq!(running(&app), Vec::<&str>::new(), "{case}: left running");
            let saved = store.load(id).await.expect(case);
            assert_eq!(saved.messages.last(), Some(&Message::User("hi".into())));
        }
    }

    #[tokio::test]
    async fn quitting_never_starts_a_turn_held_for_a_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = saving(&Store::at(dir.path()), async { Ok(()) });
        app.start_turn("go".into());
        assert!(app.turns.held().is_some(), "held for the save");

        app.stop_background_for_exit().await;

        assert!(!app.is_busy(), "the held turn started");
        // Dropped with the app; the prompt history has it.
        assert_eq!(app.turns.held(), Some(&Queued::Prompt("go".into())));
    }

    #[tokio::test]
    async fn quitting_writes_the_history_of_a_prompt_it_dropped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nth/prompt-history.jsonl");
        let history = History::load(path.clone()).await;
        let mut app = saving_with(history, &Store::at(dir.path()), async { Ok(()) });
        app.prompt.insert_str("go");
        app.submit();
        assert!(app.turns.held().is_some(), "held for the save");

        app.stop_background_for_exit().await;

        assert!(!app.history_saving.is_running(), "left writing");
        assert_eq!(std::fs::read_to_string(&path).expect("saved"), "\"go\"\n");
    }

    #[tokio::test]
    async fn a_turn_task_that_panicked_leaves_a_note() {
        let mut app = test_app();
        app.turn
            .start(|_| tokio::spawn(async { panic!("turn exploded") }));

        app.stop_background_for_exit().await;

        let [note] = app.exit_notes.as_slice() else {
            panic!("{:?}", app.exit_notes);
        };
        assert!(note.starts_with("the turn task failed: "), "{note}");
        assert!(note.ends_with(LAST_SAVED), "{note}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_turn_that_will_not_stop_is_aborted_with_a_note() {
        let mut app = test_app();
        app.turn.start(|_| tokio::spawn(std::future::pending()));
        let turn = app.turn.abort_handle().expect("running");

        app.stop_background_for_exit().await;

        assert!(!app.is_busy());
        tokio::task::yield_now().await;
        assert!(turn.is_finished(), "aborted");
        assert_eq!(
            app.exit_notes,
            [format!("the turn did not stop within 5 s; {LAST_SAVED}")]
        );
    }

    #[tokio::test]
    async fn quitting_with_monitors_running_asks_twice() {
        let mut app = test_app();
        start(&mut app, "ci").await;

        app.apply(Action::ClearOrQuit);
        assert!(!app.quit);
        let hint = app.hint.as_deref().unwrap_or_default();
        assert!(
            hint.starts_with("1 monitor running · ctrl+c again"),
            "{hint}"
        );
        app.apply(Action::Insert('x'));
        app.apply(Action::ClearOrQuit);
        app.apply(Action::ClearOrQuit);
        assert!(!app.quit, "another key in between starts over");
        app.apply(Action::ClearOrQuit);
        assert!(app.quit);
    }

    #[tokio::test]
    async fn quitting_stops_the_monitors_and_tells_the_session() {
        let mut app = test_app();
        let m = start(&mut app, "ci").await;
        // The monitor's task, which reports once stopped.
        let monitors = app.monitors.clone();
        tokio::spawn(async move {
            m.stop.cancelled().await;
            let end = MonitorEnd::Stopped(monitors.stopped_by(m.id));
            monitors
                .event(MonitorEvent::Ended {
                    id: m.id,
                    end,
                    events: 0,
                })
                .await;
        });

        app.stop_background_for_exit().await;

        let session = app.session.as_ref().expect("idle");
        let Some(Message::User(notice)) = session.messages.last() else {
            panic!("no notice saved");
        };
        assert!(notice.contains("ended=\"stopped: nth exited\""), "{notice}");
    }

    #[tokio::test]
    async fn quitting_saves_an_end_the_app_had_not_heard_yet() {
        let mut app = test_app();
        let m = start(&mut app, "ci").await;
        let end = MonitorEnd::Exited(Some(0));
        app.monitors
            .event(MonitorEvent::Ended {
                id: m.id,
                end,
                events: 0,
            })
            .await;
        assert_eq!(app.monitors.running(), 0, "the registry is done with it");

        app.stop_background_for_exit().await;

        let session = app.session.as_ref().expect("idle");
        let Some(Message::User(notice)) = session.messages.last() else {
            panic!("no notice saved");
        };
        assert!(notice.contains("ended=\"exited with code 0\""), "{notice}");
    }
}
