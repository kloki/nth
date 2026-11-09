//! A single replaceable, session-owned monotonic wakeup.
use std::time::Duration;

use tokio::{sync::watch, time::Instant};

/// Clones share one deadline. The default is disabled; an owner enables it
/// with `new` and keeps a subscription to react to changes.
#[derive(Clone, Debug, Default)]
pub struct Wakeup(Option<watch::Sender<Option<Instant>>>);

impl Wakeup {
    pub fn new() -> Self {
        Self(Some(watch::channel(None).0))
    }

    /// Subscribe before inspecting the deadline to avoid missing changes.
    pub fn subscribe(&self) -> Option<watch::Receiver<Option<Instant>>> {
        self.0.as_ref().map(watch::Sender::subscribe)
    }

    /// Replace the deadline relative to now. Failed schedules leave it intact.
    pub fn schedule(&self, duration: Duration) -> Result<Instant, &'static str> {
        let sender = self.0.as_ref().ok_or("wakeups are unavailable")?;
        if duration.is_zero() {
            return Err("duration must be positive");
        }
        let deadline = Instant::now()
            .checked_add(duration)
            .ok_or("duration is too large")?;
        sender.send_replace(Some(deadline));
        Ok(deadline)
    }

    /// Atomically take a deadline only if it is due at the time of the claim.
    pub fn claim(&self) -> bool {
        let Some(sender) = &self.0 else { return false };
        sender.send_if_modified(|deadline| {
            if deadline.is_some_and(|at| at <= Instant::now()) {
                *deadline = None;
                true
            } else {
                false
            }
        })
    }

    /// Discard any pending deadline and notify subscribers if it changed.
    pub fn clear(&self) {
        if let Some(sender) = &self.0 {
            sender.send_if_modified(|deadline| deadline.take().is_some());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replacement_notification_claim_and_clear() {
        let wakeup = Wakeup::new();
        let mut changes = wakeup.subscribe().unwrap();
        assert_eq!(*changes.borrow_and_update(), None);
        let first = wakeup.schedule(Duration::from_secs(60)).unwrap();
        changes.changed().await.unwrap();
        assert_eq!(*changes.borrow_and_update(), Some(first));
        assert!(!wakeup.claim());
        let second = wakeup.schedule(Duration::from_millis(1)).unwrap();
        assert_eq!(*changes.borrow_and_update(), Some(second));
        tokio::time::sleep_until(second).await;
        assert!(wakeup.clone().claim());
        assert!(!wakeup.claim());
        changes.changed().await.unwrap();
        assert_eq!(*changes.borrow_and_update(), None);
        wakeup.schedule(Duration::from_secs(1)).unwrap();
        wakeup.clear();
        assert_eq!(*changes.borrow_and_update(), None);
    }

    #[test]
    fn disabled_and_invalid_schedules_do_not_change_state() {
        assert!(Wakeup::default().schedule(Duration::from_secs(1)).is_err());
        assert!(Wakeup::default().subscribe().is_none());
        let wakeup = Wakeup::new();
        let deadline = wakeup.schedule(Duration::from_secs(1)).unwrap();
        assert!(wakeup.schedule(Duration::ZERO).is_err());
        assert!(wakeup.schedule(Duration::MAX).is_err());
        assert_eq!(*wakeup.subscribe().unwrap().borrow(), Some(deadline));
    }
}
