//! The one provider the app's tests run turns against: a script of
//! replies, then one reply for every request past it.

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use futures::{FutureExt, StreamExt, future::BoxFuture, stream::BoxStream};
use nth_protocol::{BoxError, Listing, Message, Provider, Request, StreamEvent};
use tokio::sync::Notify;

/// What one request gets.
#[derive(Clone)]
pub(crate) enum Reply {
    /// These events, then the stream ends.
    Events(Vec<StreamEvent>),
    /// The request fails with this error.
    Fail(String),
    /// Never answers, until the request is dropped.
    Hang,
    /// These events once the gate opens: a model mid-work.
    HangUntil(Arc<Notify>, Vec<StreamEvent>),
}

impl Reply {
    /// A reply of `text` alone.
    pub(crate) fn text(text: &str) -> Self {
        Reply::Events(vec![StreamEvent::TextDelta(text.into())])
    }
}

/// Answers each request with the next scripted reply, then every later one
/// with the fallback, recording the model and messages each request
/// carried.
pub(crate) struct Scripted {
    replies: Mutex<VecDeque<Reply>>,
    fallback: Reply,
    requests: Mutex<Vec<Vec<Message>>>,
    models: Mutex<Vec<String>>,
    /// Told whenever a request reaches the provider; a permit, so a test
    /// that looks after the request arrived still sees it.
    pub(crate) asked: Arc<Notify>,
    /// Set once a request that hangs is dropped.
    pub(crate) dropped: Arc<AtomicBool>,
}

impl Default for Scripted {
    /// Answers every request with "ok".
    fn default() -> Self {
        Self::replying([])
    }
}

impl Scripted {
    /// The scripted replies' events in turn, then "ok".
    pub(crate) fn new(replies: impl IntoIterator<Item = Vec<StreamEvent>>) -> Self {
        Self::replying(replies.into_iter().map(Reply::Events))
    }

    /// `replies` in turn, then "ok".
    pub(crate) fn replying(replies: impl IntoIterator<Item = Reply>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().collect()),
            fallback: Reply::text("ok"),
            requests: Mutex::new(Vec::new()),
            models: Mutex::new(Vec::new()),
            asked: Arc::new(Notify::new()),
            dropped: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A provider that must never be asked for anything: every request
    /// fails.
    pub(crate) fn idle() -> Self {
        Self::default().with_fallback(Reply::Fail("unused".into()))
    }

    /// A provider that never answers.
    pub(crate) fn hang() -> Self {
        Self::default().with_fallback(Reply::Hang)
    }

    /// What every request past the script gets.
    pub(crate) fn with_fallback(mut self, fallback: Reply) -> Self {
        self.fallback = fallback;
        self
    }

    /// The messages of every request, in the order they were asked.
    pub(crate) fn sent(&self) -> Vec<Vec<Message>> {
        self.requests.lock().expect("lock").clone()
    }

    /// The model of every request, in the order they were asked.
    pub(crate) fn asked(&self) -> Vec<String> {
        self.models.lock().expect("lock").clone()
    }
}

/// Sets its flag when dropped, which a hanging request is only when its
/// turn lets go of it.
struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

type Events = BoxStream<'static, Result<StreamEvent, BoxError>>;

fn events(events: Vec<StreamEvent>) -> Events {
    futures::stream::iter(events.into_iter().map(Ok)).boxed()
}

impl Provider for Scripted {
    fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
        async { Ok(Listing::default()) }.boxed()
    }

    fn stream<'a>(&'a self, request: Request<'a>) -> BoxFuture<'a, Result<Events, BoxError>> {
        self.requests
            .lock()
            .expect("lock")
            .push(request.messages.to_vec());
        self.models
            .lock()
            .expect("lock")
            .push(request.model.to_string());
        self.asked.notify_one();
        let reply = self
            .replies
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| self.fallback.clone());
        match reply {
            Reply::Events(reply) => async move { Ok(events(reply)) }.boxed(),
            Reply::Fail(error) => async move { Err(error.into()) }.boxed(),
            Reply::Hang => {
                let guard = SetOnDrop(self.dropped.clone());
                async move {
                    let _guard = guard;
                    std::future::pending().await
                }
                .boxed()
            }
            Reply::HangUntil(gate, reply) => {
                let guard = SetOnDrop(self.dropped.clone());
                async move {
                    let _guard = guard;
                    gate.notified().await;
                    Ok(events(reply))
                }
                .boxed()
            }
        }
    }
}
