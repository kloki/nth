//! One step's reply: streamed as it arrives, and retried with backoff on
//! a transient provider error.

use std::{borrow::Cow, time::Duration};

use futures::{StreamExt, stream::BoxStream};
use nth_protocol::{AssistantMessage, BoxError, Event, Request, StreamEvent, ToolSpec, Usage};

use super::{Turn, emit, max_steps_messages};
use crate::compaction::effective;

/// The first retry waits this long; each next one doubles it.
const RETRY_INITIAL_DELAY: Duration = Duration::from_secs(2);

/// The longest computed backoff, for when the server asked to wait no
/// particular time.
const RETRY_MAX_DELAY: Duration = Duration::from_secs(30);

/// How many times one request may be retried before giving up.
const RETRY_MAX_ATTEMPTS: u32 = 5;

/// A server asking to wait longer than this is not retried: the error shows
/// right away rather than an unexplained stall.
const RETRY_MAX_AFTER: Duration = Duration::from_secs(60);

/// Why a step's reply did not finish.
pub(crate) enum Stop {
    /// The provider failed; whether a retry can help is the provider's say.
    Failed(BoxError),
    /// Cancelled; the partial reply is handed back so it can be kept.
    Cancelled(AssistantMessage),
}

impl Turn<'_> {
    /// One step's request, retried on a transient provider error with
    /// backoff: 2 s, doubling, at most `RETRY_MAX_DELAY`, or the server's
    /// `Retry-After` when it gave one no longer than `RETRY_MAX_AFTER`. The
    /// retry is announced as an [`Event::Retry`], and cancelling during the
    /// wait ends the step. A failure — opening the stream or in the middle
    /// of it — retries the whole attempt, so the reply starts over and the
    /// partial one is never saved.
    pub(crate) async fn stream_step(
        &mut self,
        last: bool,
        specs: &[ToolSpec],
        last_usage: &mut Option<Usage>,
    ) -> Result<AssistantMessage, Stop> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let error = match self.stream_attempt(last, specs, last_usage).await {
                Err(Stop::Failed(error)) => error,
                done => return done,
            };
            let Some(delay) = self.retry_delay(&error, attempt) else {
                return Err(Stop::Failed(error));
            };
            if !self.wait_to_retry(attempt, delay).await {
                return Err(Stop::Cancelled(AssistantMessage::default()));
            }
        }
    }

    /// One whole attempt at the reply: opening the stream and streaming it
    /// to the end. A failure from either is the caller's to retry, from
    /// scratch: text already streamed stays on screen but the partial reply
    /// is dropped, as [`Event::Retry`] says. What the attempt reports it
    /// used is counted to the turn's spend.
    async fn stream_attempt(
        &mut self,
        last: bool,
        specs: &[ToolSpec],
        last_usage: &mut Option<Usage>,
    ) -> Result<AssistantMessage, Stop> {
        let (events, cancel) = (self.events, self.cancel);
        let effective = effective(self.messages);
        let sent = if last {
            Cow::Owned(max_steps_messages(&effective))
        } else {
            effective
        };
        let request = Request {
            model: self.route.model,
            session_id: self.route.session_id,
            parent_session_id: self.route.parent_session_id,
            effort: self.route.effort,
            messages: &sent,
            tools: specs,
        };
        let mut stream = self.open_once(request).await?;
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
                StreamEvent::Usage(usage) => {
                    *last_usage = Some(usage);
                    self.spend.steps += 1;
                    self.spend.tokens += usage;
                    emit(events, Event::Usage(usage)).await;
                }
            }
        }
        Ok(reply)
    }

    /// How long to wait before retrying `error` as the `attempt`-th try,
    /// when the provider says a retry can help; `None` when it cannot, or
    /// when its `Retry-After` is longer than `RETRY_MAX_AFTER` (the error
    /// shows right away rather than an unexplained stall).
    pub(crate) fn retry_delay(&self, error: &BoxError, attempt: u32) -> Option<Duration> {
        self.provider
            .retry(error)
            .filter(|_| attempt <= RETRY_MAX_ATTEMPTS)
            .map(|retry| retry.after.unwrap_or_else(|| backoff(attempt)))
            .filter(|delay| *delay <= RETRY_MAX_AFTER)
    }

    /// Announces a retry and waits out its delay; `false` when cancelled
    /// during the wait, which ends the caller's request.
    pub(crate) async fn wait_to_retry(&self, attempt: u32, delay: Duration) -> bool {
        emit(self.events, Event::Retry { attempt, delay }).await;
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => false,
            _ = tokio::time::sleep(delay) => true,
        }
    }

    /// One attempt at opening `request`'s stream; cancelling ends it.
    pub(crate) async fn open_once(
        &self,
        request: Request<'_>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, BoxError>>, Stop> {
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => Err(Stop::Cancelled(AssistantMessage::default())),
            stream = self.provider.stream(request) => stream.map_err(Stop::Failed),
        }
    }
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
        ToolContext, Usage,
    };
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::agent_loop::{Error, Turn, tests::*};

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
            StreamEvent::Usage(Usage {
                input: 40,
                output: 2,
                ..Usage::default()
            }),
            StreamEvent::TextDelta("partial".into()),
            StreamEvent::ToolCall(call("1", "echo", "")),
        ]);
        let cancel = CancellationToken::new();
        let ctx = ToolContext::new(".".into());
        let (tx, mut rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        let mut spent = spend();
        let mut last_usage = None;
        let turn = Turn {
            provider: &provider,
            route: ROUTE,
            tools: &[],
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spent,
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &cancel,
        }
        .run();
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
        assert_eq!((spent.steps, spent.tokens.input), (1, 40), "still counted");
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

        let mut last_usage = None;
        Turn {
            provider: &provider,
            route: ROUTE,
            tools: &[],
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spend(),
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &CancellationToken::new(),
        }
        .run()
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

    /// Streams a text delta and then fails, like a dropped connection; a
    /// retried request answers whole.
    struct DropsMidway {
        attempts: Mutex<usize>,
    }

    impl Provider for DropsMidway {
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
            let first = *attempts == 1;
            async move {
                let events: Vec<Result<StreamEvent, BoxError>> = match first {
                    true => vec![
                        Ok(StreamEvent::TextDelta("partial".into())),
                        Err(Box::new(Temporary) as BoxError),
                    ],
                    false => vec![Ok(StreamEvent::TextDelta("the whole answer".into()))],
                };
                Ok(stream::iter(events).boxed())
            }
            .boxed()
        }

        fn retry(&self, error: &BoxError) -> Option<Retry> {
            error.downcast_ref::<Temporary>().map(|_| Retry {
                // A retry that waits in a test would only slow it down.
                after: Some(Duration::ZERO),
            })
        }
    }

    /// A failure in the middle of a reply retries the whole attempt: what
    /// was already streamed is not saved, and the second attempt replaces
    /// it, as `Event::Retry` promises.
    #[tokio::test]
    async fn a_mid_stream_failure_restarts_the_step_from_scratch() {
        let provider = DropsMidway {
            attempts: Mutex::new(0),
        };
        let ctx = ToolContext::new(".".into());
        let (tx, mut rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        let mut last_usage = None;
        Turn {
            provider: &provider,
            route: ROUTE,
            tools: &[],
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spend(),
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &CancellationToken::new(),
        }
        .run()
        .await
        .expect("the retry answers");

        assert_eq!(
            messages[1..],
            [Message::Assistant(AssistantMessage {
                text: "the whole answer".into(),
                ..Default::default()
            })],
            "the second, complete attempt is what is saved"
        );
        assert_eq!(*provider.attempts.lock().expect("not poisoned"), 2);
        drop(tx);
        let mut deltas = Vec::new();
        let mut retries = Vec::new();
        while let Some(event) = rx.recv().await {
            match event {
                Event::TextDelta(text) => deltas.push(text),
                Event::Retry { attempt, delay } => retries.push((attempt, delay)),
                _ => {}
            }
        }
        assert_eq!(
            deltas,
            ["partial", "the whole answer"],
            "the partial deltas were shown, then the reply started over"
        );
        assert_eq!(retries, [(1, Duration::ZERO)], "one retry announced");
    }

    #[tokio::test]
    async fn gives_up_after_the_retry_budget() {
        let provider = Flaky::new((1..=20).collect());
        let ctx = ToolContext::new(".".into());
        let (tx, mut rx) = mpsc::channel(16);
        let mut messages = vec![Message::User("go".into())];

        let mut last_usage = None;
        let result = Turn {
            provider: &provider,
            route: ROUTE,
            tools: &[],
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spend(),
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &CancellationToken::new(),
        }
        .run()
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
