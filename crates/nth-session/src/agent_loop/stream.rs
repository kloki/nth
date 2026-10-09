//! One step's reply: streamed as it arrives, and retried with backoff on
//! a transient provider error.

use std::time::Duration;

use futures::StreamExt;
use nth_protocol::{AssistantMessage, BoxError, Event, Provider, Request, StreamEvent};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::emit;

/// The first retry waits this long; each next one doubles it.
const RETRY_INITIAL_DELAY: Duration = Duration::from_secs(2);

/// The longest computed backoff, for when the server asked to wait no
/// particular time.
const RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

/// How many times one step's request may be retried before giving up.
const RETRY_MAX_ATTEMPTS: u32 = 5;

/// A server asking to wait longer than this is not retried: the error shows
/// right away rather than an unexplained stall.
const RETRY_MAX_AFTER: Duration = Duration::from_secs(60);

/// Why a step's reply did not finish.
pub(super) enum Stop {
    /// The provider failed; whether a retry can help is the provider's say.
    Failed(BoxError),
    /// Cancelled; the partial reply is handed back so it can be kept.
    Cancelled(AssistantMessage),
}

/// One step's request, retried on a transient provider error with backoff:
/// 2 s, doubling, at most `RETRY_MAX_DELAY`, or the server's `Retry-After`
/// when it gave one no longer than `RETRY_MAX_AFTER`. The retry is announced
/// as an [`Event::Retry`], and cancelling during the wait ends the step.
pub(super) async fn stream_step(
    provider: &dyn Provider,
    request: Request<'_>,
    events: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
) -> Result<AssistantMessage, Stop> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let error = match one_attempt(provider, request, events, cancel).await {
            Err(Stop::Failed(error)) => error,
            done => return done,
        };
        let delay = provider
            .retry(&error)
            .filter(|_| attempt <= RETRY_MAX_ATTEMPTS)
            .map(|retry| retry.after.unwrap_or_else(|| backoff(attempt)))
            .filter(|delay| *delay <= RETRY_MAX_AFTER);
        let Some(delay) = delay else {
            return Err(Stop::Failed(error));
        };
        emit(events, Event::Retry { attempt, delay }).await;
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Stop::Cancelled(AssistantMessage::default())),
            _ = tokio::time::sleep(delay) => {}
        }
    }
}

/// Streams one reply, appending text, reasoning and tool calls to it as they
/// arrive.
async fn one_attempt(
    provider: &dyn Provider,
    request: Request<'_>,
    events: &mpsc::Sender<Event>,
    cancel: &CancellationToken,
) -> Result<AssistantMessage, Stop> {
    let mut stream = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(Stop::Cancelled(AssistantMessage::default())),
        stream = provider.stream(request) => stream.map_err(Stop::Failed)?,
    };
    let mut reply = AssistantMessage::default();
    loop {
        let event = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                // Calls from an unfinished reply never ran, so they are
                // dropped rather than left without results.
                reply.tool_calls.clear();
                return Err(Stop::Cancelled(reply));
            }
            event = stream.next() => event,
        };
        let Some(event) = event else { break };
        match event.map_err(Stop::Failed)? {
            StreamEvent::TextDelta(text) => {
                reply.text.push_str(&text);
                emit(events, Event::TextDelta(text)).await;
            }
            StreamEvent::ReasoningDelta(text) => {
                reply.reasoning.push_str(&text);
                emit(events, Event::ReasoningDelta(text)).await;
            }
            StreamEvent::ToolCall(call) => reply.tool_calls.push(call),
            StreamEvent::Usage(usage) => emit(events, Event::Usage(usage)).await,
        }
    }
    Ok(reply)
}

/// The backoff before the `attempt`-th retry: `RETRY_INITIAL_DELAY` doubled
/// each time, capped at `RETRY_MAX_DELAY`.
fn backoff(attempt: u32) -> Duration {
    let factor = 1u32 << (attempt.saturating_sub(1)).min(16);
    (RETRY_INITIAL_DELAY * factor).min(RETRY_MAX_DELAY)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::{
        FutureExt, StreamExt,
        future::BoxFuture,
        stream::{self, BoxStream},
    };
    use nth_protocol::{
        AssistantMessage, BoxError, Event, Listing, Message, Provider, Request, Retry, StreamEvent,
        ToolContext,
    };
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::agent_loop::{Error, run_turn, tests::*};

    /// Streams `events` and then stays open, like a reply cut off mid-way.
    struct Unfinished(Vec<StreamEvent>);

    impl Provider for Unfinished {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            async { Ok(Listing::default()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            let events = self.0.clone();
            async move {
                Ok(stream::iter(events.into_iter().map(Ok))
                    .chain(stream::pending())
                    .boxed())
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn interrupting_a_reply_keeps_its_text_but_not_its_calls() {
        let provider = Unfinished(vec![
            StreamEvent::TextDelta("partial".into()),
            StreamEvent::ToolCall(call("1", "echo", "")),
        ]);
        let cancel = CancellationToken::new();
        let ctx = ToolContext::new(".".into());
        let (tx, mut rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        let turn = run_turn(&provider, ROUTE, &[], &ctx, &mut messages, &tx, &cancel);
        let interrupt = async {
            rx.recv().await;
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(turn, interrupt);

        assert!(matches!(result, Err(Error::Interrupted)));
        assert_eq!(
            messages[1..],
            [Message::Assistant(AssistantMessage {
                text: "partial".into(),
                ..Default::default()
            })]
        );
    }

    /// A transient provider failure, retried by [`Flaky`].
    #[derive(Debug, thiserror::Error)]
    #[error("temporary failure")]
    struct Temporary;

    /// Fails the requests named in `fail` (1-based) and answers the rest.
    struct Flaky {
        fail: Vec<usize>,
        attempts: Mutex<usize>,
        reply: Vec<StreamEvent>,
        retry_after: Option<Duration>,
    }

    impl Flaky {
        fn new(fail: Vec<usize>) -> Self {
            Self {
                fail,
                attempts: Mutex::new(0),
                // A retry that waits in a test would only slow it down.
                reply: vec![StreamEvent::TextDelta("done".into())],
                retry_after: Some(Duration::ZERO),
            }
        }
    }

    impl Provider for Flaky {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            async { Ok(Listing::default()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            let mut attempts = self.attempts.lock().expect("not poisoned");
            *attempts += 1;
            let fails = self.fail.contains(&*attempts);
            let reply = self.reply.clone();
            async move {
                match fails {
                    true => Err(Box::new(Temporary) as BoxError),
                    false => Ok(stream::iter(reply.into_iter().map(Ok)).boxed()),
                }
            }
            .boxed()
        }

        fn retry(&self, error: &BoxError) -> Option<Retry> {
            error.downcast_ref::<Temporary>().map(|_| Retry {
                after: self.retry_after,
            })
        }
    }

    #[tokio::test]
    async fn retries_a_transient_error_then_answers() {
        let provider = Flaky::new(vec![1]);
        let ctx = ToolContext::new(".".into());
        let (tx, mut rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        run_turn(
            &provider,
            ROUTE,
            &[],
            &ctx,
            &mut messages,
            &tx,
            &CancellationToken::new(),
        )
        .await
        .expect("turn completes");

        assert_eq!(
            messages[1..],
            [Message::Assistant(AssistantMessage {
                text: "done".into(),
                ..Default::default()
            })]
        );
        assert_eq!(*provider.attempts.lock().expect("not poisoned"), 2);
        drop(tx);
        let mut retries = Vec::new();
        while let Some(event) = rx.recv().await {
            if let Event::Retry { attempt, delay } = event {
                retries.push((attempt, delay));
            }
        }
        assert_eq!(retries, [(1, Duration::ZERO)]);
    }

    #[tokio::test]
    async fn gives_up_after_the_retry_budget() {
        let provider = Flaky::new((1..=20).collect());
        let ctx = ToolContext::new(".".into());
        let (tx, mut rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        let result = run_turn(
            &provider,
            ROUTE,
            &[],
            &ctx,
            &mut messages,
            &tx,
            &CancellationToken::new(),
        )
        .await;

        assert!(matches!(result, Err(Error::Provider(_))));
        assert_eq!(
            *provider.attempts.lock().expect("not poisoned"),
            RETRY_MAX_ATTEMPTS as usize + 1,
            "the first try plus the retries"
        );
        drop(tx);
        let mut retries = 0;
        while let Some(event) = rx.recv().await {
            retries += usize::from(matches!(event, Event::Retry { .. }));
        }
        assert_eq!(retries, RETRY_MAX_ATTEMPTS as usize);
    }

    #[tokio::test]
    async fn a_long_retry_after_fails_at_once() {
        let provider = Flaky {
            retry_after: Some(RETRY_MAX_AFTER + Duration::from_secs(1)),
            ..Flaky::new(vec![1])
        };
        let ctx = ToolContext::new(".".into());
        let mut messages = vec![Message::User("go".into())];

        let (result, _) = turn(&provider, &[], &ctx, &mut messages).await;

        assert!(matches!(result, Err(Error::Provider(_))));
        assert_eq!(*provider.attempts.lock().expect("not poisoned"), 1);
    }
}
