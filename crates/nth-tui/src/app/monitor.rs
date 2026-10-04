//! The commands the model left running: their notices start a turn when
//! the app is idle, and leaving a session stops them.

use nth_protocol::{MonitorEvent, monitor_log_dir};

use super::{App, NOTICE_DELAY};

impl App {
    pub(super) fn on_monitor(&mut self, _event: MonitorEvent) {
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
    /// resume: they were watching for that conversation.
    pub(super) fn left_session(&mut self) {
        self.monitors.forget_all();
        self.notices_due = None;
        self.hold_notices = false;
        self.log_monitors_for_session();
    }

    pub(super) fn log_monitors_for_session(&self) {
        if let Some(session) = &self.session {
            let dir = monitor_log_dir(&self.monitor_root, &session.id.to_string());
            self.monitors.set_log_dir(dir);
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
