//! Quitting: asked twice while something runs in the background, which
//! is stopped on the way out so a resumed session knows it is gone.

use std::time::Duration;

use nth_protocol::{Message, StoppedBy};

use super::App;

/// How long quitting waits for the stopped monitors to report back.
const EXIT_WAIT: Duration = Duration::from_secs(2);

impl App {
    /// Quits, but with monitors or subagents running only on the second
    /// ask in a row.
    pub(super) fn ask_quit(&mut self) {
        let monitors = self.monitors.running();
        let subagents = self.running_subagents();
        if monitors + subagents == 0 || self.quit_armed {
            self.quit = true;
            return;
        }
        self.quit_armed = true;
        let mut running = Vec::new();
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

    /// Stops every monitor and subagent as nth quits, and saves how they
    /// ended in the session, so the model knows they are gone when it is
    /// resumed.
    pub(super) async fn stop_background_for_exit(&mut self) {
        self.stop_subagents_for_exit().await;
        if self.monitors.running() > 0 {
            self.monitors.stop_all(StoppedBy::Exit);
            let monitors = self.monitors.clone();
            let rx = &mut self.monitor_rx;
            let ended = async { while monitors.running() > 0 && rx.recv().await.is_some() {} };
            let _ = tokio::time::timeout(EXIT_WAIT, ended).await;
        }
        // Mid-turn the session is in the turn task, which quitting drops.
        let Some(session) = &mut self.session else {
            return;
        };
        let spent = self.subagents.take_spent();
        let notices = self.inbox.take_notices();
        if spent.is_empty() && notices.is_none() {
            return;
        }
        session.usage.extend(spent);
        session.messages.extend(notices.map(Message::User));
        if let Some(store) = &self.store {
            // Nowhere left to show a failure.
            let _ = store.save(session).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::{MonitorEnd, MonitorEvent};

    use super::*;
    use crate::app::{keys::Action, monitor::tests::start, tests::app};

    #[tokio::test]
    async fn quitting_with_monitors_running_asks_twice() {
        let mut app = app();
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
        let mut app = app();
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
}
