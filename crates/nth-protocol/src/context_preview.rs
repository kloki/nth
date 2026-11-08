//! Shared latest logical request snapshot, readable by tools and front-ends.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::watch;

/// One serialized snapshot of the session preview.
pub type Snapshot = Arc<serde_json::Value>;

/// The newest committed snapshot nth-session published. Clones share one
/// slot; a reader that falls behind skips to the newest.
#[derive(Debug, Clone)]
pub struct ContextPreview {
    slot: watch::Sender<Option<Snapshot>>,
    /// Something asked for a snapshot without subscribing: the `context` tool.
    wanted: Arc<AtomicBool>,
}

/// No receiver is kept, so a headless context builds nothing until
/// something subscribes or asks.
impl Default for ContextPreview {
    fn default() -> Self {
        let (slot, _) = watch::channel(None);
        Self {
            slot,
            wanted: Arc::default(),
        }
    }
}

impl ContextPreview {
    pub fn get(&self) -> Option<Snapshot> {
        self.slot.borrow().clone()
    }

    pub fn publish(&self, snapshot: Snapshot) {
        self.slot.send_replace(Some(snapshot));
    }

    /// A reader of every later snapshot. Building one costs O(history), so
    /// a front-end holds this only while a turn it may show runs, never
    /// between turns.
    pub fn subscribe(&self) -> watch::Receiver<Option<Snapshot>> {
        self.slot.subscribe()
    }

    /// Asks for snapshots without subscribing, for a reader that polls
    /// with [`ContextPreview::get`].
    pub fn want(&self) {
        self.wanted.store(true, Ordering::Relaxed);
    }

    /// Whether a snapshot would reach anyone; the loop builds none otherwise.
    pub fn builds(&self) -> bool {
        self.slot.receiver_count() > 0 || self.wanted.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_slow_subscriber_sees_the_newest_snapshot() {
        let preview = ContextPreview::default();
        let mut rx = preview.subscribe();
        preview.publish(Arc::new(serde_json::json!(1)));
        preview.publish(Arc::new(serde_json::json!(2)));
        rx.changed().await.expect("the slot is open");
        assert_eq!(
            rx.borrow_and_update().as_deref(),
            Some(&serde_json::json!(2))
        );
        assert!(!rx.has_changed().expect("the slot is open"));
    }

    #[test]
    fn builds_only_for_a_subscriber_or_a_wanter() {
        let preview = ContextPreview::default();
        assert!(!preview.builds());
        let rx = preview.subscribe();
        assert!(preview.clone().builds(), "clones share the slot");
        drop(rx);
        assert!(!preview.builds());
        preview.clone().want();
        assert!(preview.builds());
    }
}
