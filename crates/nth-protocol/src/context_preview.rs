//! Shared latest logical request snapshot, readable by tools and front-ends.
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::Event;

#[derive(Debug, Default)]
struct State {
    latest: Option<Arc<serde_json::Value>>,
    sending: bool,
}

/// Immutable JSON snapshots produced by nth-session. Clones share one slot.
#[derive(Debug, Clone, Default)]
pub struct ContextPreview(Arc<Mutex<State>>);

impl ContextPreview {
    pub fn get(&self) -> Option<Arc<serde_json::Value>> {
        self.0
            .lock()
            .expect("context preview lock poisoned")
            .latest
            .clone()
    }

    pub fn set(&self, snapshot: Arc<serde_json::Value>) {
        self.0.lock().expect("context preview lock poisoned").latest = Some(snapshot);
    }

    /// Publish immediately when possible. If the event channel is full, one
    /// detached sender waits for capacity and sends the latest snapshot,
    /// coalescing intervening revisions without delaying the turn.
    pub fn publish(&self, snapshot: Arc<serde_json::Value>, events: &mpsc::Sender<Event>) {
        let mut state = self.0.lock().expect("context preview lock poisoned");
        state.latest = Some(snapshot.clone());
        if state.sending {
            return;
        }
        match events.try_send(Event::ContextUpdated(snapshot)) {
            Ok(()) | Err(mpsc::error::TrySendError::Closed(_)) => return,
            Err(mpsc::error::TrySendError::Full(_)) => state.sending = true,
        }
        let shared = self.clone();
        let events = events.clone();
        tokio::spawn(async move {
            let permit = events.reserve().await;
            let mut state = shared.0.lock().expect("context preview lock poisoned");
            if let (Ok(permit), Some(snapshot)) = (permit, state.latest.clone()) {
                permit.send(Event::ContextUpdated(snapshot));
            }
            state.sending = false;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn full_channel_coalesces_and_delivers_final_snapshot() {
        let preview = ContextPreview::default();
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(Event::TextDelta("busy".into())).await.unwrap();
        preview.publish(Arc::new(serde_json::json!({"revision": 1})), &tx);
        preview.publish(Arc::new(serde_json::json!({"revision": 2})), &tx);
        assert_eq!(preview.get().unwrap()["revision"], 2);
        assert!(matches!(rx.recv().await, Some(Event::TextDelta(_))));
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert!(matches!(event, Some(Event::ContextUpdated(value)) if value["revision"] == 2));
    }

    #[tokio::test]
    async fn closing_full_channel_releases_sender() {
        let preview = ContextPreview::default();
        let (tx, rx) = mpsc::channel(1);
        tx.send(Event::TextDelta("busy".into())).await.unwrap();
        preview.publish(Arc::new(serde_json::json!(1)), &tx);
        drop(rx);
        tokio::task::yield_now().await;
        assert!(!preview.0.lock().unwrap().sending);
    }

    #[tokio::test]
    async fn later_revisions_follow_a_deferred_delivery() {
        let preview = ContextPreview::default();
        let (tx, mut rx) = mpsc::channel(1);
        tx.send(Event::TextDelta("busy".into())).await.unwrap();
        preview.publish(Arc::new(serde_json::json!(1)), &tx);
        rx.recv().await;
        let first = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap();
        assert!(matches!(first, Some(Event::ContextUpdated(value)) if *value == 1));
        preview.publish(Arc::new(serde_json::json!(2)), &tx);
        let second = rx.recv().await;
        assert!(matches!(second, Some(Event::ContextUpdated(value)) if *value == 2));
    }
}
