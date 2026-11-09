//! The active session's one-shot model wakeup.

use tokio::time::Instant;

use super::App;

impl App {
    pub(crate) fn wakeup_deadline(&self) -> Option<Instant> {
        *self.wakeup_rx.borrow()
    }

    pub(super) fn reset_wakeup(&mut self) {
        self.wakeup.clear();
        self.wakeup = nth_protocol::Wakeup::new();
        self.wakeup_rx = self.wakeup.subscribe().expect("enabled wakeup");
    }

    pub(super) fn wakeup_due(&mut self) {
        if self.wakeup.claim() {
            self.inbox.post_wakeup();
            self.context_stale();
            self.arm_notices();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::app::tests::test_app;

    #[tokio::test(start_paused = true)]
    async fn replacement_escape_and_leaving_discard_old_deadlines() {
        let mut app = test_app();
        let old = app.wakeup.clone();
        old.schedule(Duration::from_secs(2)).unwrap();
        old.schedule(Duration::from_secs(4)).unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;
        app.wakeup_due();
        assert!(!app.inbox.wakes());
        app.interrupt();
        assert!(app.wakeup_deadline().is_none());
        old.schedule(Duration::from_secs(1)).unwrap();
        app.left_session();
        old.schedule(Duration::from_secs(1)).unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;
        app.wakeup_due();
        assert!(!app.inbox.wakes());
        assert!(app.wakeup_deadline().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_posts_once_and_arms_existing_notice_queue() {
        let mut app = test_app();
        app.wakeup.schedule(Duration::from_secs(1)).unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        app.wakeup_due();
        app.wakeup_due();
        assert!(app.turns.due().is_some());
        let notice = app.inbox.take_notices().unwrap();
        assert_eq!(notice.matches("<wakeup").count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn escape_discards_expired_wakeup_but_preserves_other_notices() {
        let mut app = test_app();
        app.inbox.monitor_line(1, "build", std::path::Path::new("/log"), "done".into());
        app.wakeup.schedule(Duration::from_secs(1)).unwrap();
        tokio::time::advance(Duration::from_secs(1)).await;
        app.wakeup_due();
        app.interrupt();
        let notice = app.inbox.take_notices().unwrap();
        assert!(notice.contains("done"));
        assert!(!notice.contains("<wakeup"));
    }

    #[tokio::test]
    async fn interrupted_turn_cannot_leave_a_late_schedule_armed() {
        let mut app = test_app();
        app.start_turn("go".into());
        app.interrupt();
        app.wakeup.schedule(Duration::from_secs(10)).unwrap();
        app.inbox.post_wakeup();
        let ended = app.turn.join().await.unwrap();
        app.end_turn(ended);
        assert!(app.wakeup_deadline().is_none());
        assert!(!app.inbox.wakes());
    }
}
