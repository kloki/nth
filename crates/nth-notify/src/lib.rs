//! Desktop notifications: what nth tells you when you are looking elsewhere.
//! `Event::notification` writes the text from `templates/`, a `Backend`
//! shows it, and `Notifier` is the handle a front-end sends through without
//! waiting. `notify-send` is the only backend today; another is one module
//! and one `BackendKind` variant.

mod config;
mod message;
mod notify_send;

use std::sync::Arc;

pub use config::{BackendKind, NotifyConfig};
use futures::future::BoxFuture;
pub use message::{Context, Event};
pub use notify_send::NotifySend;
use tokio::sync::{mpsc, watch};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// The one line every notification daemon shows.
    pub summary: String,
    pub body: String,
}

pub trait Backend: Send + Sync {
    /// As in the config, for messages about it.
    fn name(&self) -> &'static str;

    fn send<'a>(&'a self, notification: &'a Notification) -> BoxFuture<'a, Result<(), BoxError>>;
}

/// Notifications this many behind are dropped rather than make the sender
/// wait: a late one says nothing useful.
const QUEUE: usize = 8;

/// A cheap-to-clone handle to one task that sends notifications in order.
/// The task ends once every handle is gone.
#[derive(Clone)]
pub struct Notifier {
    tx: Option<mpsc::Sender<Notification>>,
    /// The backend's first failure, held here too so `off` has a sender and
    /// `errors` never reports a closed channel.
    error: Arc<watch::Sender<Option<String>>>,
}

impl Notifier {
    /// Needs a Tokio runtime: it spawns the sending task.
    pub fn new(backend: Box<dyn Backend>) -> Self {
        let (tx, mut rx) = mpsc::channel::<Notification>(QUEUE);
        let error = Arc::new(watch::Sender::new(None));
        let failed = error.clone();
        tokio::spawn(async move {
            while let Some(notification) = rx.recv().await {
                if let Err(e) = backend.send(&notification).await {
                    // Only the first: the same failure every turn is noise.
                    failed.send_if_modified(|first| {
                        first.is_none() && {
                            *first = Some(format!("{}: {e}", backend.name()));
                            true
                        }
                    });
                }
            }
        });
        Self {
            tx: Some(tx),
            error,
        }
    }

    /// Sends nothing: notifications turned off, and tests.
    pub fn off() -> Self {
        Self {
            tx: None,
            error: Arc::new(watch::Sender::new(None)),
        }
    }

    /// Never waits; see `QUEUE`.
    pub fn notify(&self, notification: Notification) {
        if let Some(tx) = &self.tx {
            let _ = tx.try_send(notification);
        }
    }

    /// Changes once, to the first error the backend gave.
    pub fn errors(&self) -> watch::Receiver<Option<String>> {
        self.error.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct Recording {
        sent: Arc<Mutex<Vec<String>>>,
        fail: bool,
    }

    impl Backend for Recording {
        fn name(&self) -> &'static str {
            "recording"
        }

        fn send<'a>(&'a self, n: &'a Notification) -> BoxFuture<'a, Result<(), BoxError>> {
            Box::pin(async move {
                if self.fail {
                    return Err(format!("no {}", n.summary).into());
                }
                self.sent.lock().unwrap().push(n.summary.clone());
                Ok(())
            })
        }
    }

    fn notification(summary: &str) -> Notification {
        Notification {
            summary: summary.into(),
            body: String::new(),
        }
    }

    #[tokio::test]
    async fn sends_in_order() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let notifier = Notifier::new(Box::new(Recording {
            sent: sent.clone(),
            fail: false,
        }));
        notifier.notify(notification("one"));
        notifier.notify(notification("two"));
        drop(notifier);
        // The task drains the queue before it sees the channel close.
        for _ in 0..100 {
            if sent.lock().unwrap().len() == 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(*sent.lock().unwrap(), ["one", "two"]);
    }

    #[tokio::test]
    async fn reports_only_the_first_error() {
        let notifier = Notifier::new(Box::new(Recording {
            fail: true,
            ..Default::default()
        }));
        let mut errors = notifier.errors();
        notifier.notify(notification("one"));
        notifier.notify(notification("two"));
        errors.changed().await.unwrap();
        assert_eq!(
            errors.borrow_and_update().as_deref(),
            Some("recording: no one")
        );
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(!errors.has_changed().unwrap());
    }

    #[tokio::test]
    async fn off_keeps_its_error_channel_open() {
        let notifier = Notifier::off();
        notifier.notify(notification("one"));
        assert!(!notifier.errors().has_changed().unwrap());
    }
}
