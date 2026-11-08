//! When a client counts as idle: the touches using it right now, and when
//! the last one ended, which the reaper stops it by.

use std::{
    sync::{Arc, Weak},
    time::Duration,
};

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::{
    Entry, Inner, Key, Slot,
    usage::{Caller, snapshot},
};
use crate::client::Client;

impl Entry {
    /// A touch starts using this client.
    pub(super) fn enter(&mut self, caller: Option<&Caller>) {
        self.busy += 1;
        self.usage.enter(caller);
    }

    /// A touch is done with it: the idle time counts from now.
    fn leave(&mut self, caller: Option<&Caller>) {
        self.busy -= 1;
        self.touched = Instant::now();
        self.usage.leave(caller);
    }
}

/// One touch's use of an entry, from asking for its client until the touch
/// ends, however it ends: a cancelled turn drops it too.
pub(super) struct Busy {
    pub(super) inner: Arc<Inner>,
    pub(super) key: Key,
    /// Which entry it counts in: one evicted while busy may have been
    /// replaced under the same key, whose count is not this touch's.
    pub(super) slot: Slot,
    pub(super) caller: Option<Caller>,
}

impl Drop for Busy {
    fn drop(&mut self) {
        let usage = {
            let mut clients = self
                .inner
                .clients
                .lock()
                .expect("lsp client map lock poisoned");
            let Some(entry) = clients
                .get_mut(&self.key)
                .filter(|entry| entry.slot.ptr_eq(&self.slot))
            else {
                return;
            };
            entry.leave(self.caller.as_ref());
            snapshot(&clients)
        };
        self.inner.publish(usage);
    }
}

/// Stops `client` once its entry has gone `idle` without a touch: the entry
/// and its status go, so the next touch starts the server again. Holds the
/// pool only weakly, and ends with it.
pub(super) async fn stop_when_idle(
    inner: Weak<Inner>,
    key: Key,
    client: Arc<Client>,
    idle: Duration,
    cancel: CancellationToken,
) {
    let mut deadline = Instant::now() + idle;
    let (inner, usage) = loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = tokio::time::sleep_until(deadline) => {}
        }
        let Some(inner) = inner.upgrade() else { return };
        let mut clients = inner.clients.lock().expect("lsp client map lock poisoned");
        let Some(entry) = clients.get(&key) else {
            return;
        };
        deadline = match entry.busy {
            0 => entry.touched + idle,
            _ => Instant::now() + idle,
        };
        if deadline > Instant::now() {
            continue;
        }
        clients.remove(&key);
        // Under the lock, so a start for the same key that follows cannot
        // have its status taken away.
        inner
            .status
            .send_modify(|all| all.retain(|s| s.id != key.0 || s.root != key.1));
        let usage = snapshot(&clients);
        drop(clients);
        break (inner, usage);
    };
    inner.publish(usage);
    drop(inner);
    client.shutdown().await;
    cancel.cancel();
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::{
        ServerState,
        fake::{scripted_idling, state_of},
    };

    #[tokio::test]
    async fn an_idle_server_is_stopped_and_started_again_on_touch() {
        let (_tmp, file, lsp) = scripted_idling(&[], Some(Duration::from_millis(200)));
        let mut status = lsp.status();

        assert_eq!(lsp.touch(&file, true).await[&file][0].message, "boom");
        assert_eq!(state_of(&status.borrow()), Some(ServerState::Connected));

        tokio::time::timeout(Duration::from_secs(5), status.wait_for(|s| s.is_empty()))
            .await
            .expect("the idle server was stopped")
            .unwrap();
        assert!(lsp.inner.clients.lock().unwrap().is_empty());

        assert_eq!(lsp.touch(&file, true).await[&file][0].message, "boom");
        assert_eq!(state_of(&status.borrow()), Some(ServerState::Connected));
    }

    #[tokio::test]
    async fn a_touch_outlasting_the_idle_time_keeps_its_server() {
        let (tmp, _file, lsp) = scripted_idling(&[], Some(Duration::from_millis(100)));
        let status = lsp.status();
        let server = lsp.inner.servers.iter().find(|s| s.id == "fake").unwrap();

        // A touch that is still waiting on its client long after the idle
        // time is up, as a slow start or a long wait for diagnostics is.
        let (busy, client) = lsp
            .client(server.clone(), tmp.path().to_path_buf(), None)
            .await;
        assert!(client.is_some());
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(state_of(&status.borrow()), Some(ServerState::Connected));

        // Once it ends, the idle time counts from then.
        drop(busy);
        let mut status = lsp.status();
        tokio::time::timeout(Duration::from_secs(5), status.wait_for(|s| s.is_empty()))
            .await
            .expect("the idle server was stopped")
            .unwrap();
    }

    #[tokio::test]
    async fn a_server_kept_busy_is_not_stopped() {
        let (_tmp, file, lsp) = scripted_idling(&[], Some(Duration::from_millis(300)));
        let status = lsp.status();

        lsp.touch(&file, true).await;
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            lsp.touch(&file, false).await;
            assert_eq!(state_of(&status.borrow()), Some(ServerState::Connected));
        }
    }
}
