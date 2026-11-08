//! The commands the model left running: a tab each, what they print put
//! in the model's inbox, their notices start a turn when the app is idle,
//! and leaving a session or quitting stops them. A monitor's tab closes by
//! itself once its process has stopped, and only then, so nothing keeps
//! running out of sight; the log keeps its output.

use std::collections::BTreeMap;

use nth_protocol::{MonitorEvent, MonitorId, Stream};

use super::{App, NOTICE_DELAY, Tab};
use crate::monitor::MonitorView;

/// The monitors' tabs, and which monitors the model still hears from.
#[derive(Default)]
pub(super) struct MonitorTabs {
    /// Each monitor's tab, open from its start until its process stops.
    pub(super) views: BTreeMap<MonitorId, MonitorView>,
    /// Monitors numbered up to here ran for a session that was left: what
    /// they still say shows in their tabs but never reaches the next
    /// session's model, which has no notice for them to join. A number
    /// rather than a set of live ones, since a monitor's start may still
    /// wait in the channel when its session is left.
    forgotten: MonitorId,
}

impl App {
    pub(super) fn on_monitor(&mut self, event: MonitorEvent) {
        match event {
            MonitorEvent::Started {
                id,
                description,
                command,
                log,
            } => {
                self.monitor_tabs
                    .views
                    .insert(id, MonitorView::new(description, command, log));
                // Opened, not shown: you keep looking at the chat while the
                // model works.
                self.content.add(Tab::Monitor(id));
            }
            MonitorEvent::Output { id, line, stream } => {
                let Some(view) = self.monitor_tabs.views.get_mut(&id) else {
                    return;
                };
                // Stderr is for the tab and the log only. Stdout goes into
                // the inbox, which the idle preview lists as pending.
                let heard = stream == Stream::Stdout && id > self.monitor_tabs.forgotten;
                if heard {
                    self.inbox
                        .monitor_line(id, &view.description, &view.log, line.clone());
                }
                view.push(stream, line);
                if heard {
                    self.context_stale();
                }
            }
            MonitorEvent::Ended { id, end, events } => {
                if let Some(view) = self.monitor_tabs.views.remove(&id)
                    && id > self.monitor_tabs.forgotten
                {
                    self.inbox
                        .monitor_ended(id, &view.description, &view.log, end, events);
                }
                self.context_stale();
                self.content.remove(Tab::Monitor(id));
            }
        }
        self.arm_notices();
    }

    /// Starts the wait before notices wake the model, unless one runs
    /// already: a burst keeps its first deadline rather than putting it off
    /// line by line.
    pub(super) fn arm_notices(&mut self) {
        if self.notices_due.is_none() && self.inbox.wakes() {
            self.notices_due = Some(tokio::time::Instant::now() + NOTICE_DELAY);
        }
    }

    /// Wakes the agent with what its monitors said, unless a turn is
    /// running, which hands them over itself, or you stopped the last one.
    pub(super) fn notices_due(&mut self) {
        self.notices_due = None;
        if !self.is_busy() && !self.hold_notices && self.inbox.wakes() {
            self.start_turn(String::new());
        }
    }

    /// Stops the monitors of a session that was left, after `/clear` or a
    /// resume: they were watching for that conversation. Their tabs close
    /// as each process stops, as they always do.
    pub(super) fn left_session(&mut self) {
        // A turn held for a save, and prompts still in the inbox, were sent
        // to the session left: they go back to the prompt before the inbox
        // is cleared.
        self.unqueue();
        self.areas.forget(&crate::app::mouse::Target::PullRequest);
        self.reset_curios();
        self.left_subagents();
        self.monitor_tabs.forgotten = self.monitors.forget_all();
        self.inbox.clear();
        self.notices_due = None;
        self.hold_notices = false;
        self.log_monitors_for_session();
    }

    /// Points the monitors started from now on at the session's folder in
    /// the store.
    pub(super) fn log_monitors_for_session(&self) {
        if let Some(store) = &self.store {
            self.monitors
                .set_log_dir(store.monitor_dir(self.session_id));
        }
    }

    /// Whether the app has yet to hear a monitor of this session end: the
    /// registry stops counting one as its end is sent, before the app
    /// hears it, so the registry alone cannot say every end is in the inbox.
    pub(super) fn awaits_monitor_ends(&self) -> bool {
        self.monitor_tabs
            .views
            .keys()
            .any(|id| *id > self.monitor_tabs.forgotten)
    }

    /// Monitors whose process runs, as far as the app has heard: each has
    /// a tab until it stops.
    pub(crate) fn running_monitors(&self) -> usize {
        self.monitor_tabs.views.len()
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
pub(super) mod tests {
    use nth_protocol::{MonitorEnd, MonitorId, Registered, StoppedBy, Stream};
    use ratatui::style::Color;

    use super::*;
    use crate::app::{
        keys::Action,
        tests::{app, rows, tab_colour},
    };

    /// Starts a monitor as the tool would, and lets the app hear of it.
    pub(in crate::app) async fn start(app: &mut App, description: &str) -> Registered {
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
        assert!(rows[0].starts_with(" [› chat] $ ci "), "{}", rows[0]);
        assert_eq!(tab_colour(&mut app, "$ ci"), Color::Blue, "running");
    }

    #[tokio::test]
    async fn the_tab_closes_once_the_process_exits() {
        let mut app = app();
        let m = start(&mut app, "ci").await;
        start(&mut app, "logs").await;
        app.apply(Action::Content(1));

        ended(&mut app, m.id, MonitorEnd::Exited(Some(0))).await;
        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Monitor(2)]);
        assert_eq!(app.content.active(), Tab::Chat, "the one before shows");
        assert!(!app.monitor_tabs.views.contains_key(&m.id));
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
        let hint = app.hint.as_deref().unwrap_or_default();
        assert!(
            hint.starts_with("still running · ctrl+w stops it first"),
            "{hint}"
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
        assert_eq!(app.content.tabs(), [Tab::Chat]);
        assert!(app.monitor_tabs.views.is_empty());
    }

    #[tokio::test]
    async fn clear_closes_the_tabs_as_their_processes_stop() {
        let mut app = app();
        let running = start(&mut app, "ci").await;

        app.run_command(crate::command::Command::Clear);
        assert!(running.stop.is_cancelled());
        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::Monitor(1)]);

        ended(&mut app, running.id, MonitorEnd::Stopped(StoppedBy::Exit)).await;
        assert_eq!(app.content.tabs(), [Tab::Chat]);
        assert!(!app.inbox.has_notices(), "not for the new session");
    }

    #[tokio::test]
    async fn a_stdout_line_reaches_the_inbox_and_stderr_does_not() {
        let mut app = app();
        app.monitors.set_log_dir("/logs".into());
        let m = start(&mut app, "errors \"in\" log").await;
        say(&mut app, m.id, "ERROR one", Stream::Stdout).await;
        say(&mut app, m.id, "noise", Stream::Stderr).await;
        say(&mut app, m.id, "ERROR two", Stream::Stdout).await;

        assert!(app.inbox.wakes());
        assert!(app.notices_due.is_some(), "a turn is on its way");
        assert_eq!(
            app.inbox.take_notices().unwrap(),
            "<monitor id=\"1\" description=\"errors &quot;in&quot; log\" log=\"/logs/1.log\">\n\
             ERROR one\nERROR two\n</monitor>"
        );
    }

    #[tokio::test]
    async fn an_ended_monitor_closes_its_notice() {
        let mut app = app();
        app.monitors.set_log_dir("/logs".into());
        let m = start(&mut app, "ci").await;
        say(&mut app, m.id, "build ok", Stream::Stdout).await;
        app.monitors
            .event(MonitorEvent::Ended {
                id: m.id,
                end: MonitorEnd::Exited(Some(1)),
                events: 1,
            })
            .await;
        hear(&mut app);

        assert_eq!(app.monitors.running(), 0);
        assert_eq!(
            app.inbox.take_notices().unwrap(),
            "<monitor id=\"1\" description=\"ci\" log=\"/logs/1.log\">\nbuild ok\n</monitor>\n\
             <monitor id=\"1\" description=\"ci\" log=\"/logs/1.log\" ended=\"exited with code 1\" events=\"1\"/>"
        );
    }

    #[tokio::test]
    async fn a_forgotten_monitors_late_end_posts_nothing() {
        let mut app = app();
        let m = start(&mut app, "ci").await;
        say(&mut app, m.id, "before", Stream::Stdout).await;

        app.run_command(crate::command::Command::Clear);
        assert!(m.stop.is_cancelled());
        say(&mut app, m.id, "after", Stream::Stdout).await;
        ended(&mut app, m.id, MonitorEnd::Stopped(StoppedBy::Exit)).await;

        assert!(!app.inbox.has_notices());
        assert!(app.notices_due.is_none());
        assert_eq!(app.content.tabs(), [Tab::Chat], "the tab still heard it");
    }

    #[tokio::test]
    async fn a_monitor_whose_start_was_still_queued_stays_with_its_session() {
        let mut app = app();
        // Started and printing before the app heard of it, then the
        // session is left: the old registry forgot it under its lock, so
        // nothing it says may reach the next session's model.
        let m = app
            .monitors
            .register("ci", "watch")
            .await
            .expect("front-end");
        let output = MonitorEvent::Output {
            id: m.id,
            line: "late".into(),
            stream: Stream::Stdout,
        };
        app.monitors.event(output).await;
        app.run_command(crate::command::Command::Clear);
        hear(&mut app);
        ended(&mut app, m.id, MonitorEnd::Stopped(StoppedBy::Exit)).await;
        assert!(!app.inbox.has_notices());

        let next = start(&mut app, "next").await;
        say(&mut app, next.id, "heard", Stream::Stdout).await;
        assert!(app.inbox.wakes(), "the next session's monitors still speak");
    }

    #[tokio::test]
    async fn monitors_log_beside_the_session_save() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = nth_session::Store::at(dir.path());
        let mut app = app().with_store(store.clone());
        let first = start(&mut app, "ci").await;
        assert_eq!(first.log, store.monitor_dir(app.session_id).join("1.log"));

        app.run_command(crate::command::Command::Clear);
        let second = start(&mut app, "ci").await;
        assert_eq!(second.log, store.monitor_dir(app.session_id).join("2.log"));
        assert_ne!(first.log.parent(), second.log.parent(), "a folder each");
    }
}
