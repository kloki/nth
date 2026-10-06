//! The commands the model left running: a tab each, their notices start a
//! turn when the app is idle, and leaving a session or quitting stops them.
//! A monitor's tab only closes once its process has stopped, so nothing
//! keeps running out of sight.

use std::time::Duration;

use nth_protocol::{Message, MonitorEvent, StoppedBy, monitor_log_dir};

use super::{App, NOTICE_DELAY, Tab};
use crate::monitor::MonitorView;

/// How long quitting waits for the stopped monitors to report back.
const EXIT_WAIT: Duration = Duration::from_secs(2);

impl App {
    pub(super) fn on_monitor(&mut self, event: MonitorEvent) {
        match event {
            MonitorEvent::Started {
                id,
                description,
                command,
                log,
            } => {
                self.monitor_views
                    .insert(id, MonitorView::new(description, command, log));
                // Opened, not shown: you keep looking at the chat while the
                // model works.
                self.content.add(Tab::Monitor(id));
            }
            MonitorEvent::Output { id, line, stream } => {
                if let Some(view) = self.monitor_views.get_mut(&id) {
                    view.push(stream, line);
                }
            }
            MonitorEvent::Ended { id, end, .. } => {
                if let Some(view) = self.monitor_views.get_mut(&id) {
                    view.end(end);
                    if view.closing {
                        self.monitor_views.remove(&id);
                        self.content.remove(Tab::Monitor(id));
                    }
                }
            }
        }
        if self.notices_due.is_none() && self.monitors.has_notices() {
            self.notices_due = Some(tokio::time::Instant::now() + NOTICE_DELAY);
        }
    }

    /// Wakes the agent with what its monitors said, unless a turn is
    /// running, which hands them over itself, or you stopped the last one.
    pub(super) fn notices_due(&mut self) {
        self.notices_due = None;
        if !self.is_busy() && !self.hold_notices && self.monitors.has_notices() {
            self.start_turn(String::new());
        }
    }

    /// Stops the monitors of a session that was left, after `/clear` or a
    /// resume: they were watching for that conversation. Their tabs close
    /// as each process stops.
    pub(super) fn left_session(&mut self) {
        self.monitors.forget_all();
        self.notices_due = None;
        self.hold_notices = false;
        self.log_monitors_for_session();
        let mut stopped = Vec::new();
        for (&id, view) in &mut self.monitor_views {
            if view.is_running() {
                view.closing = true;
            } else {
                stopped.push(id);
            }
        }
        for id in stopped {
            self.monitor_views.remove(&id);
            self.content.remove(Tab::Monitor(id));
        }
    }

    pub(super) fn log_monitors_for_session(&self) {
        if let Some(session) = &self.session {
            let dir = monitor_log_dir(&self.monitor_root, &session.id.to_string());
            self.monitors.set_log_dir(dir);
        }
    }

    /// Monitors whose process runs, as far as their tabs have heard.
    pub(crate) fn running_monitors(&self) -> usize {
        self.monitor_views
            .values()
            .filter(|v| v.is_running())
            .count()
    }

    pub(super) fn tab_label(&self, tab: Tab) -> String {
        match tab {
            Tab::Chat => "chat".into(),
            Tab::Diagnostics => "diagnostics".into(),
            Tab::Plan => self.plan.label(),
            Tab::Monitor(id) => self
                .monitor_views
                .get(&id)
                .map_or_else(|| format!("monitor {id}"), MonitorView::label),
        }
    }

    /// ctrl+w: stops the monitor showing, or closes its tab once stopped.
    pub(super) fn stop_content(&mut self) {
        let Tab::Monitor(id) = self.content.active() else {
            return;
        };
        match self.monitor_views.get(&id) {
            Some(view) if view.is_running() => {
                self.monitors.stop(id, StoppedBy::User);
            }
            _ => self.close_content(),
        }
    }

    /// Closes the tab showing, unless it is a monitor still running or the
    /// plan, which shows while there is one.
    pub(super) fn close_content(&mut self) {
        if self.content.active() == Tab::Plan && self.plan.exists() {
            self.hint = Some("the plan tab stays while there is a plan".into());
            return;
        }
        if let Tab::Monitor(id) = self.content.active() {
            if self
                .monitor_views
                .get(&id)
                .is_some_and(MonitorView::is_running)
            {
                self.hint = Some("still running · ctrl+w stops it first".into());
                return;
            }
            self.monitor_views.remove(&id);
        }
        self.content.close();
    }

    /// Quits, but with monitors running only on the second ask in a row.
    pub(super) fn ask_quit(&mut self) {
        let running = self.monitors.running();
        if running == 0 || self.quit_armed {
            self.quit = true;
            return;
        }
        self.quit_armed = true;
        let monitors = if running == 1 { "monitor" } else { "monitors" };
        self.hint = Some(format!(
            "{running} {monitors} running · ctrl+c again to quit"
        ));
    }

    /// Stops every monitor as nth quits, and saves how they ended in the
    /// session, so the model knows they are gone when it is resumed.
    pub(super) async fn stop_monitors_for_exit(&mut self) {
        if self.monitors.running() == 0 {
            return;
        }
        self.monitors.stop_all(StoppedBy::Exit);
        let monitors = self.monitors.clone();
        let rx = &mut self.monitor_rx;
        let ended = async { while monitors.running() > 0 && rx.recv().await.is_some() {} };
        let _ = tokio::time::timeout(EXIT_WAIT, ended).await;
        // Mid-turn the session is in the turn task, which quitting drops.
        let Some(session) = &mut self.session else {
            return;
        };
        let Some(notices) = self.monitors.take_notices() else {
            return;
        };
        session.messages.push(Message::User(notices));
        if let Some(store) = &self.store {
            // Nowhere left to show a failure.
            let _ = store.save(session).await;
        }
    }
}

/// Resolves at `at`, or never without one, so the loop's arm sleeps.
pub(super) async fn due(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::{MonitorEnd, MonitorId, Registered, Stream};

    use super::*;
    use crate::app::{
        keys::Action,
        tests::{app, rows},
    };

    /// Starts a monitor as the tool would, and lets the app hear of it.
    async fn start(app: &mut App, description: &str) -> Registered {
        let m = app
            .monitors
            .register(description, "watch")
            .await
            .expect("front-end");
        hear(app);
        m
    }

    async fn say(app: &mut App, id: MonitorId, line: &str, stream: Stream) {
        let output = MonitorEvent::Output {
            id,
            line: line.into(),
            stream,
        };
        app.monitors.event(output).await;
        hear(app);
    }

    async fn ended(app: &mut App, id: MonitorId, end: MonitorEnd) {
        app.monitors
            .event(MonitorEvent::Ended { id, end, events: 0 })
            .await;
        hear(app);
    }

    fn hear(app: &mut App) {
        while let Ok(event) = app.monitor_rx.try_recv() {
            app.on_monitor(event);
        }
    }

    #[tokio::test]
    async fn a_monitor_opens_a_tab_without_showing_it() {
        let mut app = app();
        start(&mut app, "ci").await;

        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Monitor(1)]);
        assert_eq!(app.content.active(), Tab::Chat);
        let rows = rows(&mut app);
        assert!(rows[0].starts_with(" 1 chat  2 ● ci"), "{}", rows[0]);
        assert!(rows[15].ends_with("» 1 monitor "), "{}", rows[15]);
    }

    #[tokio::test]
    async fn the_tab_shows_the_command_and_its_lines() {
        let mut app = app();
        let m = start(&mut app, "ci").await;
        say(&mut app, m.id, "step 1 ok", Stream::Stdout).await;
        say(&mut app, m.id, "warning", Stream::Stderr).await;
        app.apply(Action::Content(1));

        let rows = rows(&mut app);
        assert_eq!(rows[2].trim_end(), " $ watch");
        assert!(
            rows[3].starts_with(" running · 0s · 1 event · "),
            "{}",
            rows[3]
        );
        assert_eq!(rows[5].trim_end(), " step 1 ok");
        assert_eq!(rows[6].trim_end(), " warning");
    }

    #[tokio::test]
    async fn a_running_monitor_tab_does_not_close() {
        let mut app = app();
        start(&mut app, "ci").await;
        app.apply(Action::Content(1));

        app.apply(Action::CloseContent);
        assert_eq!(app.content.active(), Tab::Monitor(1));
        let rows = rows(&mut app);
        assert!(
            rows[15].starts_with(" still running · ctrl+w stops it first"),
            "{}",
            rows[15]
        );
        app.run_command(crate::command::Command::Close);
        assert_eq!(app.content.active(), Tab::Monitor(1), "nor with /close");
    }

    #[tokio::test]
    async fn ctrl_w_stops_the_monitor_then_closes_its_tab() {
        let mut app = app();
        let m = start(&mut app, "ci").await;
        app.apply(Action::Content(1));

        app.apply(Action::StopContent);
        assert!(m.stop.is_cancelled());
        assert_eq!(app.monitors.stopped_by(m.id), StoppedBy::User);
        assert_eq!(app.content.active(), Tab::Monitor(1), "until it stopped");

        ended(&mut app, m.id, MonitorEnd::Stopped(StoppedBy::User)).await;
        assert!(rows(&mut app)[0].contains("2 ✗ ci"));
        app.apply(Action::StopContent);
        assert_eq!(app.content.tabs(), [Tab::Chat]);
        assert!(app.monitor_views.is_empty());
    }

    #[tokio::test]
    async fn ctrl_q_closes_a_stopped_monitor_tab() {
        let mut app = app();
        let m = start(&mut app, "ci").await;
        ended(&mut app, m.id, MonitorEnd::Exited(Some(0))).await;
        app.apply(Action::Content(1));

        app.apply(Action::CloseContent);
        assert_eq!(app.content.tabs(), [Tab::Chat]);
    }

    #[tokio::test]
    async fn clear_closes_the_tabs_as_their_processes_stop() {
        let mut app = app();
        let running = start(&mut app, "ci").await;
        let done = start(&mut app, "logs").await;
        ended(&mut app, done.id, MonitorEnd::Exited(Some(0))).await;

        app.run_command(crate::command::Command::Clear);
        assert!(running.stop.is_cancelled());
        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Monitor(1)]);

        ended(&mut app, running.id, MonitorEnd::Stopped(StoppedBy::Exit)).await;
        assert_eq!(app.content.tabs(), [Tab::Chat]);
        assert!(!app.monitors.has_notices(), "not for the new session");
    }

    #[tokio::test]
    async fn quitting_with_monitors_running_asks_twice() {
        let mut app = app();
        start(&mut app, "ci").await;

        app.apply(Action::ClearOrQuit);
        assert!(!app.quit);
        let rows = rows(&mut app);
        assert!(
            rows[15].starts_with(" 1 monitor running · ctrl+c again"),
            "{}",
            rows[15]
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

        app.stop_monitors_for_exit().await;

        let session = app.session.as_ref().expect("idle");
        let Some(Message::User(notice)) = session.messages.last() else {
            panic!("no notice saved");
        };
        assert!(notice.contains("ended=\"stopped: nth exited\""), "{notice}");
    }
}
