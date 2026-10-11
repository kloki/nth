//! The turn the model asked for later with `schedule_wakeup`: one at most,
//! kept on the app until it fires, the session is left or nth quits. When
//! it fires, its prompt goes into the inbox as a notice, and the inbox
//! starts the turn as it does for monitors.

use std::time::Duration;

use jiff::{Timestamp, Zoned};
use nth_protocol::{AlarmRequest, WakeupRequest, clock};

use super::{App, NOTICE_DELAY};

/// The longest the loop sleeps before it looks at the wall clock again. The
/// loop's clock stops while the machine sleeps; the wall clock does not, so
/// a wakeup due during a suspend fires soon after the resume.
const RECHECK: Duration = Duration::from_secs(30);

impl App {
    /// Sets or drops the wakeup, answering with the one it replaced.
    pub(super) fn on_alarm(&mut self, (request, reply): AlarmRequest) {
        let old = match request {
            WakeupRequest::Set(wakeup) => self.wakeup.replace(wakeup),
            WakeupRequest::Cancel => self.wakeup.take(),
        };
        // The tool is gone if its turn was stopped; the change stands.
        let _ = reply.send(old);
    }

    /// When the loop next looks whether the wakeup is due, if one waits.
    pub(super) fn wakeup_check(&self) -> Option<tokio::time::Instant> {
        let wakeup = self.wakeup.as_ref()?;
        let left = Duration::try_from(wakeup.at.duration_since(Timestamp::now()))
            .unwrap_or(Duration::ZERO);
        Some(tokio::time::Instant::now() + left.min(RECHECK))
    }

    /// Fires the wakeup if it is due: its notice goes in the inbox, and
    /// starts a turn unless one runs, which hands it over between steps.
    /// It starts one even after you stopped the last turn: you or the
    /// model asked for this one.
    pub(super) fn wakeup_due(&mut self) {
        if self.wakeup.as_ref().is_none_or(|w| w.at > Timestamp::now()) {
            return;
        }
        let Some(wakeup) = self.wakeup.take() else {
            return;
        };
        self.inbox
            .post_wakeup(wakeup.reason, clock(&Zoned::now()), wakeup.prompt);
        self.hold_notices = false;
        if self.notices_due.is_none() {
            self.notices_due = Some(tokio::time::Instant::now() + NOTICE_DELAY);
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;
    use nth_protocol::{Alarm, Wakeup};

    use super::*;
    use crate::app::tests::{app, rows};

    fn wakeup(reason: &str, in_secs: i64) -> Wakeup {
        Wakeup {
            at: Timestamp::now() + SignedDuration::from_secs(in_secs),
            prompt: "look".into(),
            reason: reason.into(),
        }
    }

    #[tokio::test]
    async fn the_tool_sets_replaces_and_cancels_the_one_wakeup() {
        let mut app = app();
        let alarm = Alarm::new(app.alarm_tx.clone());
        let set = tokio::spawn({
            let alarm = alarm.clone();
            async move { alarm.set(wakeup("ci", 600)).await }
        });
        let request = app.alarm_rx.recv().await.expect("asked");
        app.on_alarm(request);
        assert_eq!(set.await.unwrap(), Some(None), "nothing replaced");

        let set = tokio::spawn({
            let alarm = alarm.clone();
            async move { alarm.set(wakeup("deploy", 60)).await }
        });
        let request = app.alarm_rx.recv().await.expect("asked");
        app.on_alarm(request);
        let replaced = set.await.unwrap().flatten().expect("replaced");
        assert_eq!(replaced.reason, "ci");
        assert_eq!(
            app.wakeup.as_ref().map(|w| w.reason.as_str()),
            Some("deploy")
        );

        let cancel = tokio::spawn(async move { alarm.cancel().await });
        let request = app.alarm_rx.recv().await.expect("asked");
        app.on_alarm(request);
        assert!(cancel.await.unwrap().flatten().is_some());
        assert_eq!(app.wakeup, None);
    }

    #[tokio::test]
    async fn the_status_bar_shows_the_pending_wakeup() {
        let mut app = app();
        app.wakeup = Some(wakeup("ci", 600));

        let rows = rows(&mut app);
        let status = rows.last().expect("rows");
        assert!(status.starts_with(" ◷ "), "{status}");
        assert!(status.contains(" · ci"), "{status}");
    }

    #[tokio::test]
    async fn it_fires_only_once_due_and_clear_drops_it() {
        let mut app = app();
        app.wakeup = Some(wakeup("ci", 600));
        let check = app.wakeup_check().expect("pending");
        assert!(
            check <= tokio::time::Instant::now() + RECHECK,
            "looks again within {RECHECK:?}"
        );

        app.wakeup_due();
        assert!(app.wakeup.is_some(), "not due yet");
        assert!(!app.inbox.has_notices());

        app.run_command(crate::command::Command::Clear);
        assert_eq!(app.wakeup, None, "it was for the session left");
    }
}
