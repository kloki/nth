//! One compaction: summarizing the conversation since the last
//! [`Message::Compaction`] into a new one appended at the end, which keeps
//! the newest messages beside the summary as text. Later requests start at
//! it (see `stream::effective`).

use futures::StreamExt;
use nth_protocol::{
    AssistantMessage, BoxError, ContextOverflow, Event, Message, Request, StreamEvent, Usage,
};

use super::{Error, Stop, Turn, emit};
use crate::compaction::{self, SHRINK_STEPS};

impl Turn<'_> {
    /// Whether the next step should summarize instead of asking the model:
    /// the session has automatic compaction on, knows the model's window
    /// with room left under the reserved buffer, and the last finished
    /// reply's usage reached the usable part of it.
    pub(super) fn compaction_due(&self, last_usage: Option<Usage>) -> bool {
        let compaction = &self.route.compaction;
        if !compaction.auto {
            return false;
        }
        let Some(usable) = compaction.usable(self.route.context_window, self.route.max_output)
        else {
            return false;
        };
        // Nothing fits a window that is all reserved buffer: a request
        // cannot be made smaller than the summary it would start with, so
        // compacting could never help.
        usable > 0 && last_usage.is_some_and(|usage| usage.context() >= usable)
    }

    /// Whether the conversation the next request would carry still fills
    /// the usable window right after a compaction. Compacting again would
    /// summarize a view that no longer shrinks — a floor like a huge system
    /// prompt — so the turn gives up instead of asking for a summary before
    /// every step.
    pub(super) fn still_over_the_window(&self) -> bool {
        let Some(usable) = self
            .route
            .compaction
            .usable(self.route.context_window, self.route.max_output)
        else {
            return false;
        };
        usable > 0 && compaction::estimate(&super::stream::effective(self.messages)) >= usable
    }

    /// Summarizes what came since the last compaction — its summary and
    /// kept text, then the messages after it — and appends a new one
    /// holding the summary and the newest messages as text, tool results
    /// cut. Nothing before the last compaction is read again, and the
    /// request after it starts on a user message. `Ok(false)` when there was
    /// nothing to summarize: a caller that must shrink the conversation
    /// cannot, and one that only tried to keeps going.
    pub async fn compact(&mut self) -> Result<bool, Error> {
        let compaction = self.route.compaction;
        let usable = compaction.usable(self.route.context_window, self.route.max_output);
        let last = self
            .messages
            .iter()
            .enumerate()
            .rev()
            .find_map(|(at, message)| match message {
                Message::Compaction { summary, recent } => {
                    Some((at, summary.clone(), recent.clone()))
                }
                _ => None,
            });
        let (since, previous, previous_recent) = match last {
            Some((at, summary, recent)) => (at + 1, Some(summary), recent),
            None => (0, None, String::new()),
        };
        let since = &self.messages[since..];
        let Some(split) = compaction::split(
            since,
            compaction.preserve(usable),
            !previous_recent.is_empty(),
        ) else {
            return Ok(false);
        };
        let exchanges = compaction::exchanges(&previous_recent, &since[..split.older]);
        if exchanges.is_empty() && previous.is_none() {
            return Ok(false);
        }
        let summary = self.summarize(previous.as_deref(), &exchanges).await?;
        self.messages.push(Message::Compaction {
            summary: summary.clone(),
            recent: split.recent,
        });
        emit(self.events, Event::Compacted { summary }).await;
        Ok(true)
    }

    /// The summary of `exchanges` folded into the `previous` summary. A
    /// request the provider refuses as too long is sent again without its
    /// oldest exchanges, aiming at a shrinking share of the refused size,
    /// as opencode does; [`Error::TooBigToCompact`] once that runs out or
    /// only the newest exchange is left.
    async fn summarize(
        &mut self,
        previous: Option<&str>,
        exchanges: &[String],
    ) -> Result<String, Error> {
        let mut first = 0;
        let mut refused = None;
        let mut steps = SHRINK_STEPS.iter();
        loop {
            let prompt = compaction::summary_request(previous, &exchanges[first..], first);
            match self.request_summary(prompt.as_str()).await {
                Err(Error::TooBigToCompact) => {}
                other => return other,
            }
            let Some(step) = steps.next() else {
                return Err(Error::TooBigToCompact);
            };
            let refused = *refused.get_or_insert(compaction::text_tokens(&prompt));
            let target = (refused as f64 * step) as u64;
            first = compaction::shrink(previous, exchanges, first, target)
                .ok_or(Error::TooBigToCompact)?;
        }
    }

    /// The summary request: one user message, no tools, streamed to the
    /// end and retried as a step's is — a failure opening it or in the
    /// middle of it restarts it from scratch, so a partial attempt is
    /// discarded and only a complete one becomes the summary. Its usage is
    /// counted like a step's, but its text is not streamed as an answer —
    /// the front-end hears of it as [`Event::Compacted`].
    async fn request_summary(&mut self, prompt: &str) -> Result<String, Error> {
        let messages = [Message::User(prompt.to_string())];
        let request = Request {
            model: self.route.model,
            session_id: self.route.session_id,
            parent_session_id: self.route.parent_session_id,
            effort: self.route.effort,
            messages: &messages,
            tools: &[],
        };
        let mut attempt = 0;
        loop {
            attempt += 1;
            let error = match self.summary_attempt(request).await {
                Err(Stop::Failed(error)) => error,
                Err(Stop::Cancelled(_)) => return Err(Error::Interrupted),
                Ok(summary) if summary.trim().is_empty() => return Err(Error::EmptyReply),
                Ok(summary) => return Ok(summary),
            };
            // The provider refused the request as too long: the same one
            // again cannot fit; the caller shrinks it.
            if error.downcast_ref::<ContextOverflow>().is_some() {
                return Err(Error::TooBigToCompact);
            }
            let Some(delay) = self.retry_delay(&error, attempt) else {
                return Err(self.request_failed(error));
            };
            if !self.wait_to_retry(attempt, delay).await {
                return Err(Error::Interrupted);
            }
        }
    }

    /// One whole attempt at the summary, streamed to the end; a failure
    /// leaves no summary behind, so the caller's retry starts it over.
    async fn summary_attempt(&mut self, request: Request<'_>) -> Result<String, Stop> {
        let (events, cancel) = (self.events, self.cancel);
        let mut stream = self.open_once(request).await?;
        let mut summary = String::new();
        loop {
            let event = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Stop::Cancelled(AssistantMessage::default())),
                event = stream.next() => event,
            };
            let Some(event) = event else { break };
            match event.map_err(Stop::Failed)? {
                StreamEvent::TextDelta(text) => summary.push_str(&text),
                StreamEvent::ReasoningDelta(_) => {}
                // No tools were offered, so none can arrive.
                StreamEvent::ToolCall(_) => {}
                StreamEvent::Usage(usage) => {
                    self.spend.steps += 1;
                    self.spend.tokens += usage;
                    emit(events, Event::Usage(usage)).await;
                }
            }
        }
        Ok(summary)
    }

    /// A failed summary request as the loop's error. One that overflowed
    /// cannot be fixed by compacting again.
    fn request_failed(&self, error: BoxError) -> Error {
        match error.downcast_ref::<ContextOverflow>() {
            Some(_) => Error::TooBigToCompact,
            None => Error::Provider(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Mutex, time::Duration};

    use futures::{
        FutureExt, StreamExt,
        future::BoxFuture,
        stream::{self, BoxStream},
    };
    use nth_protocol::{
        AssistantMessage, BoxError, Listing, Provider, Request, Retry, StreamEvent, Tool,
        ToolContext, Usage,
    };
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        Compaction, Route,
        agent_loop::tests::{Echo, ROUTE, Scripted, call, spend},
        compaction::{self, SUMMARY_PROMPT},
    };

    fn assistant(text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            text: text.into(),
            ..AssistantMessage::default()
        })
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input,
            output,
            ..Usage::default()
        }
    }

    /// A retryable provider failure that is not an overflow.
    #[derive(Debug, thiserror::Error)]
    #[error("temporary failure")]
    struct Temporary;

    /// A route with the model's window and an explicit tail budget, so the
    /// tests do not depend on estimated sizes. Usable is 10k − 2k.
    fn route(preserve: u64) -> Route<'static> {
        Route {
            context_window: Some(10_000),
            max_output: Some(2_000),
            compaction: Compaction {
                auto: true,
                reserved: 0,
                preserve,
            },
            ..ROUTE
        }
    }

    /// The summary of the newest compaction in `messages`.
    fn last_summary(messages: &[Message]) -> Option<&str> {
        messages.iter().rev().find_map(|message| match message {
            Message::Compaction { summary, .. } => Some(summary.as_str()),
            _ => None,
        })
    }

    #[tokio::test]
    async fn a_compaction_appends_a_checkpoint_keeping_the_newest_as_text() {
        let provider = Scripted::new(vec![vec![
            StreamEvent::TextDelta("# Objective\nship it".into()),
            StreamEvent::Usage(usage(100, 10)),
        ]]);
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(16);
        // An old turn too big for the budget, then a recent one kept.
        let mut messages = vec![
            Message::System("sys".into()),
            Message::User("first question".into()),
            assistant(&"x".repeat(400)),
            Message::User("second question".into()),
            assistant("second answer"),
        ];
        let before = messages.clone();
        let mut spent = spend();

        let mut last_usage = None;
        Turn {
            provider: &provider,
            route: route(40),
            tools: &[],
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spent,
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &cancel,
        }
        .compact()
        .await
        .expect("compacts");

        assert_eq!(messages[..5], before[..], "the history is kept as it was");
        assert_eq!(
            messages[5],
            Message::Compaction {
                summary: "# Objective\nship it".into(),
                recent: "[User]: second question\n\n[Assistant]: second answer".into(),
            },
            "the checkpoint is appended, keeping the newest turn as text"
        );
        assert_eq!(spent.steps, 1, "the summary request is counted");
        drop(tx);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        assert_eq!(
            events,
            [
                Event::Usage(usage(100, 10)),
                Event::Compacted {
                    summary: "# Objective\nship it".into()
                }
            ],
            "the summary request is reported, then the compaction"
        );

        let sent = provider.sent.lock().expect("not poisoned").clone();
        let [Message::User(prompt)] = &sent[0][..] else {
            panic!("one user message: {:?}", sent[0]);
        };
        assert!(
            prompt.starts_with(SUMMARY_PROMPT.trim_end()),
            "the template opens it"
        );
        assert!(prompt.contains("[User]: first question"), "{prompt}");
        assert!(
            prompt.contains(&format!("[Assistant]: {}", "x".repeat(400))),
            "the serialized answer"
        );
        assert!(
            !prompt.contains("second question"),
            "the kept part is not summarized"
        );
    }

    #[tokio::test]
    async fn usage_at_the_window_compacts_before_the_next_step() {
        let provider = Scripted::new(vec![
            vec![
                StreamEvent::TextDelta("first".into()),
                StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#)),
                StreamEvent::Usage(usage(9_000, 1_500)),
            ],
            vec![StreamEvent::TextDelta("the summary".into())],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(64);
        let mut messages = vec![
            Message::System("sys".into()),
            Message::User("old question".into()),
            assistant(&"x".repeat(400)),
            Message::User("go".into()),
        ];
        let mut spent = spend();

        let mut last_usage = None;
        Turn {
            provider: &provider,
            route: route(100),
            tools: &tools,
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spent,
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &cancel,
        }
        .run()
        .await
        .expect("turn completes");

        let recent = "[User]: go\n\n[Assistant]: first\n[Assistant tool call]: echo \
                      {\"say\":\"hi\"}\n\n[Tool result]: hi";
        assert_eq!(
            messages[6],
            Message::Compaction {
                summary: "the summary".into(),
                recent: recent.into(),
            },
            "after the step's tool results"
        );
        assert_eq!(
            messages.last(),
            Some(&assistant("done")),
            "the turn goes on after the compaction"
        );

        let sent = provider.sent.lock().expect("not poisoned").clone();
        assert_eq!(sent.len(), 3, "the step, the summary, then the step after");
        assert_eq!(
            sent[1],
            [Message::User(format!(
                "{}\n\n[User]: old question\n[Assistant]: {}",
                SUMMARY_PROMPT.trim_end(),
                "x".repeat(400)
            ))],
            "the summary request carries the template and the older part"
        );
        assert_eq!(
            sent[2],
            [
                Message::System("sys".into()),
                Message::User(compaction::checkpoint("the summary", recent)),
            ],
            "the step after starts at the checkpoint, a user message"
        );
        drop(tx);
        let mut compacted = 0;
        while let Some(event) = rx.recv().await {
            compacted += usize::from(matches!(event, Event::Compacted { .. }));
        }
        assert_eq!(compacted, 1);
    }

    /// Fails the requests numbered in `overflow` (1-based) with a context
    /// overflow, as a wire does once it recognizes its provider's overflow
    /// error, and answers the rest with `replies` in order.
    struct Overflows {
        overflow: Vec<usize>,
        replies: Mutex<Vec<Vec<StreamEvent>>>,
        sent: Mutex<Vec<Vec<Message>>>,
    }

    impl Overflows {
        fn new(overflow: Vec<usize>, replies: Vec<Vec<StreamEvent>>) -> Self {
            Self {
                overflow,
                replies: Mutex::new(replies),
                sent: Mutex::new(Vec::new()),
            }
        }
    }

    impl Provider for Overflows {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            async { Ok(Listing::default()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            request: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            let mut sent = self.sent.lock().expect("not poisoned");
            sent.push(request.messages.to_vec());
            if self.overflow.contains(&sent.len()) {
                return async { Err(Box::new(ContextOverflow) as BoxError) }.boxed();
            }
            let reply = self.replies.lock().expect("not poisoned").remove(0);
            async move { Ok(stream::iter(reply.into_iter().map(Ok)).boxed()) }.boxed()
        }
    }

    fn overflowing_conversation() -> Vec<Message> {
        vec![
            Message::System("sys".into()),
            Message::User("old question".into()),
            assistant(&"x".repeat(400)),
            Message::User("the failed prompt".into()),
        ]
    }

    #[tokio::test]
    async fn an_overflowing_step_compacts_and_goes_on_from_the_checkpoint() {
        let provider = Overflows::new(
            vec![1],
            vec![
                vec![StreamEvent::TextDelta("the summary".into())],
                vec![StreamEvent::TextDelta("done".into())],
            ],
        );
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(64);
        let mut messages = overflowing_conversation();
        let mut spent = spend();

        let mut last_usage = None;
        Turn {
            provider: &provider,
            route: route(40),
            tools: &[],
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spent,
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &cancel,
        }
        .run()
        .await
        .expect("the turn recovers");

        assert_eq!(
            messages[4..],
            [
                Message::Compaction {
                    summary: "the summary".into(),
                    recent: "[User]: the failed prompt".into(),
                },
                assistant("done")
            ],
        );

        let sent = provider.sent.lock().expect("not poisoned").clone();
        assert_eq!(sent.len(), 3, "the overflow, the summary, the step again");
        let [Message::User(prompt)] = &sent[1][..] else {
            panic!("one user message: {:?}", sent[1]);
        };
        assert!(prompt.contains("old question"), "{prompt}");
        assert!(
            !prompt.contains("the failed prompt"),
            "the prompt rides in the checkpoint, not the summary"
        );
        assert_eq!(
            sent[2],
            [
                Message::System("sys".into()),
                Message::User(compaction::checkpoint(
                    "the summary",
                    "[User]: the failed prompt"
                )),
            ]
        );
    }

    /// The overflow that shows the fix for a turn whose own tool results
    /// overflowed: they ride in the checkpoint cut, not verbatim.
    #[tokio::test]
    async fn an_overflow_from_the_turns_own_tool_results_keeps_them_cut() {
        let huge = "y".repeat(40_000);
        let provider = Overflows::new(
            vec![1],
            vec![
                vec![StreamEvent::TextDelta("the summary".into())],
                vec![StreamEvent::TextDelta("done".into())],
            ],
        );
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(64);
        let mut messages = vec![
            Message::System("sys".into()),
            Message::User("read it".into()),
            Message::Assistant(AssistantMessage {
                tool_calls: vec![call("1", "read", "{}")],
                ..AssistantMessage::default()
            }),
            Message::ToolResult {
                call_id: "1".into(),
                content: huge.clone(),
            },
        ];
        let mut spent = spend();

        let mut last_usage = None;
        Turn {
            provider: &provider,
            route: route(2_000),
            tools: &[],
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spent,
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &cancel,
        }
        .run()
        .await
        .expect("the turn recovers");

        let sent = provider.sent.lock().expect("not poisoned").clone();
        let retried = compaction::estimate(&sent[2]);
        assert!(
            retried < 1_000,
            "the step after is a fraction of the 10k it overflowed with: {retried}"
        );
        let Message::User(checkpoint) = &sent[2][1] else {
            panic!("a checkpoint: {:?}", sent[2]);
        };
        assert!(checkpoint.contains("[truncated]"), "{checkpoint}");
        assert!(!checkpoint.contains(&huge));
    }

    #[tokio::test]
    async fn a_second_overflow_in_the_same_step_gives_up() {
        let provider = Overflows::new(
            vec![1, 3],
            vec![vec![StreamEvent::TextDelta("the summary".into())]],
        );
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(64);
        let mut messages = overflowing_conversation();
        let mut spent = spend();

        let mut last_usage = None;
        let result = Turn {
            provider: &provider,
            route: route(40),
            tools: &[],
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spent,
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &cancel,
        }
        .run()
        .await;

        assert!(matches!(result, Err(Error::TooBigToCompact)), "{result:?}");
        assert_eq!(provider.sent.lock().expect("not poisoned").len(), 3);
    }

    #[tokio::test]
    async fn an_overflow_in_a_later_step_compacts_again() {
        let provider = Overflows::new(
            vec![1, 4],
            vec![
                vec![StreamEvent::TextDelta("first summary".into())],
                vec![StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#))],
                vec![StreamEvent::TextDelta("second summary".into())],
                vec![StreamEvent::TextDelta("done".into())],
            ],
        );
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(64);
        let mut messages = overflowing_conversation();
        let mut spent = spend();

        let mut last_usage = None;
        Turn {
            provider: &provider,
            route: route(40),
            tools: &tools,
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spent,
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &cancel,
        }
        .run()
        .await
        .expect("each step's overflow is recovered");

        assert_eq!(last_summary(&messages), Some("second summary"));
        assert_eq!(messages.last(), Some(&assistant("done")));
        let sent = provider.sent.lock().expect("not poisoned").clone();
        assert_eq!(sent.len(), 6);
    }

    #[tokio::test]
    async fn a_second_compaction_reads_nothing_before_the_first() {
        let provider = Scripted::new(vec![
            vec![StreamEvent::TextDelta("second summary".into())],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(64);
        let mut messages = vec![
            Message::System("sys".into()),
            Message::User("ancient history".into()),
            Message::Compaction {
                summary: "first summary".into(),
                recent: "[User]: kept before".into(),
            },
            Message::User("old question".into()),
            assistant(&"x".repeat(400)),
            Message::User("go".into()),
        ];
        let mut spent = spend();
        let mut last_usage = None;
        let mut turn = Turn {
            provider: &provider,
            route: route(40),
            tools: &[],
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spent,
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &cancel,
        };

        turn.compact().await.expect("compacts");
        assert!(turn.stream_step(false, &[], &mut None).await.is_ok());

        assert_eq!(
            messages[6],
            Message::Compaction {
                summary: "second summary".into(),
                recent: "[User]: go".into(),
            },
            "the new checkpoint is appended"
        );
        let sent = provider.sent.lock().expect("not poisoned").clone();
        let [Message::User(prompt)] = &sent[0][..] else {
            panic!("one user message: {:?}", sent[0]);
        };
        assert!(
            prompt.contains("<summary>\nfirst summary\n</summary>"),
            "the earlier summary is folded in: {prompt}"
        );
        assert!(prompt.contains(compaction::UPDATE_PROMPT.trim_end()));
        assert!(
            prompt.contains("[User]: kept before"),
            "its kept text is the oldest of the transcript"
        );
        assert!(prompt.contains("[User]: old question"));
        assert!(
            !prompt.contains("ancient history"),
            "nothing before the earlier checkpoint is read again"
        );
        assert_eq!(
            sent[1],
            [
                Message::System("sys".into()),
                Message::User(compaction::checkpoint("second summary", "[User]: go")),
            ],
            "the request starts at the newest checkpoint"
        );
    }

    #[tokio::test]
    async fn a_refused_summary_request_is_sent_again_without_its_oldest_exchanges() {
        // The summary request is refused once, then answered.
        let provider = Overflows::new(
            vec![1],
            vec![vec![StreamEvent::TextDelta("the summary".into())]],
        );
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(64);
        let mut messages = vec![
            Message::System("sys".into()),
            Message::User(format!("one {}", "o".repeat(4_000))),
            assistant("a"),
            Message::User(format!("two {}", "t".repeat(4_000))),
            assistant("b"),
            Message::User(format!("three {}", "h".repeat(4_000))),
            assistant("c"),
            Message::User("last".into()),
        ];
        let mut spent = spend();
        let mut last_usage = None;

        Turn {
            provider: &provider,
            route: route(40),
            tools: &[],
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spent,
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &cancel,
        }
        .compact()
        .await
        .expect("the smaller request lands the compaction");

        assert_eq!(last_summary(&messages), Some("the summary"));
        let sent = provider.sent.lock().expect("not poisoned").clone();
        let [Message::User(refused)] = &sent[0][..] else {
            panic!("one user message: {:?}", sent[0]);
        };
        let [Message::User(retried)] = &sent[1][..] else {
            panic!("one user message: {:?}", sent[1]);
        };
        assert!(refused.contains("[User]: one"));
        assert!(!retried.contains("[User]: one"), "the oldest exchange went");
        assert!(retried.contains("older exchange"), "and says so");
        assert!(retried.contains("[User]: three"), "the newest stays");
    }

    #[tokio::test]
    async fn a_summary_request_that_overflows_gives_up() {
        /// Refuses every request as too long.
        struct Always;

        impl Provider for Always {
            fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
                async { Ok(Listing::default()) }.boxed()
            }

            fn stream<'a>(
                &'a self,
                _: Request<'a>,
            ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
            {
                async { Err(Box::new(ContextOverflow) as BoxError) }.boxed()
            }
        }

        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(64);
        let mut messages = vec![
            Message::System("sys".into()),
            Message::User("old question".into()),
            assistant(&"x".repeat(400)),
            Message::User("the failed prompt".into()),
        ];
        let mut spent = spend();

        let mut last_usage = None;
        let result = Turn {
            provider: &Always,
            route: route(40),
            tools: &[],
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spent,
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &cancel,
        }
        .run()
        .await;

        assert!(matches!(result, Err(Error::TooBigToCompact)));
    }

    /// A conversation with a floor compacting cannot lift: every step's
    /// reply reports the request's own size as used, and the system prompt
    /// alone fills more than the usable window. The review's reproduction.
    #[derive(Default)]
    struct FullWindow {
        requests: Mutex<usize>,
        summaries: Mutex<usize>,
    }

    impl Provider for FullWindow {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            async { Ok(Listing::default()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            request: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            let n = {
                let mut requests = self.requests.lock().expect("not poisoned");
                *requests += 1;
                *requests
            };
            if request.tools.is_empty() {
                *self.summaries.lock().expect("not poisoned") += 1;
                let reply = vec![StreamEvent::TextDelta("# Objective\nstill over".into())];
                return async move { Ok(stream::iter(reply.into_iter().map(Ok)).boxed()) }.boxed();
            }
            let input = compaction::estimate(request.messages);
            // A different call each step, so the doom-loop guard does not
            // end the turn before the compaction churn shows.
            let reply = vec![
                StreamEvent::ToolCall(call(
                    &n.to_string(),
                    "echo",
                    &format!(r#"{{"say":"hi-{n}"}}"#),
                )),
                StreamEvent::Usage(usage(input, 10)),
            ];
            async move { Ok(stream::iter(reply.into_iter().map(Ok)).boxed()) }.boxed()
        }
    }

    /// Compacting cannot shrink a conversation whose system prompt alone
    /// fills the window: the turn must stop at once rather than ask for a
    /// summary before every one of its steps.
    #[tokio::test]
    async fn a_floor_over_the_window_stops_the_turn_instead_of_compacting_every_step() {
        let provider = FullWindow::default();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(64);
        // Some 15k estimated tokens, well over the 10k − 2k usable window.
        let mut messages = vec![
            Message::System("s".repeat(60_000)),
            Message::User("go".into()),
        ];
        let mut spent = spend();
        let mut last_usage = None;

        let result = Turn {
            provider: &provider,
            route: Route {
                max_steps: 4,
                ..route(100)
            },
            tools: &tools,
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spent,
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &cancel,
        }
        .run()
        .await;

        assert!(matches!(result, Err(Error::TooBigToCompact)), "{result:?}");
        assert_eq!(
            *provider.summaries.lock().expect("not poisoned"),
            1,
            "one summary, not one before each of the four steps"
        );
    }

    /// A window that is all reserved buffer has no usable room: no usage,
    /// however large, can be helped by compacting, so the trigger never
    /// fires.
    #[tokio::test]
    async fn a_window_with_no_usable_room_never_compacts() {
        let provider = FullWindow::default();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(64);
        let mut messages = vec![Message::System("sys".into()), Message::User("go".into())];
        let mut spent = spend();
        let mut last_usage = None;

        let result = Turn {
            provider: &provider,
            route: Route {
                max_steps: 2,
                // reserved = min(20k, output) = the whole window.
                context_window: Some(1_000),
                max_output: Some(1_000),
                ..route(100)
            },
            tools: &tools,
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spent,
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &cancel,
        }
        .run()
        .await;

        assert!(matches!(result, Err(Error::TooManySteps(2))), "{result:?}");
        assert_eq!(
            *provider.summaries.lock().expect("not poisoned"),
            0,
            "nothing fires with no room to compact into"
        );
        assert_eq!(
            *provider.requests.lock().expect("not poisoned"),
            2,
            "just the two steps"
        );
    }

    /// A transient failure on the summary request is retried exactly as a
    /// step's opening is, so the compaction still lands.
    #[tokio::test]
    async fn a_retryable_summary_failure_is_retried() {
        /// Fails the request named in `fail` (1-based) and answers the rest
        /// with `replies` in order, retrying at once.
        struct FlakySummary {
            fail: usize,
            attempts: Mutex<usize>,
            replies: Mutex<Vec<Vec<StreamEvent>>>,
        }

        impl Provider for FlakySummary {
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
                if *attempts == self.fail {
                    return async { Err(Box::new(Temporary) as BoxError) }.boxed();
                }
                let reply = self.replies.lock().expect("not poisoned").remove(0);
                async move { Ok(stream::iter(reply.into_iter().map(Ok)).boxed()) }.boxed()
            }

            fn retry(&self, error: &BoxError) -> Option<Retry> {
                error.downcast_ref::<Temporary>().map(|_| Retry {
                    // A retry that waits in a test would only slow it down.
                    after: Some(Duration::ZERO),
                })
            }
        }

        let provider = FlakySummary {
            // The step, then the summary; the retried summary answers.
            fail: 2,
            attempts: Mutex::new(0),
            replies: Mutex::new(vec![
                vec![
                    StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#)),
                    StreamEvent::Usage(usage(9_000, 1_500)),
                ],
                vec![StreamEvent::TextDelta("the summary".into())],
                vec![StreamEvent::TextDelta("done".into())],
            ]),
        };
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(64);
        let mut messages = vec![
            Message::System("sys".into()),
            Message::User("old question".into()),
            assistant(&"x".repeat(400)),
            Message::User("go".into()),
        ];
        let mut spent = spend();
        let mut last_usage = None;

        Turn {
            provider: &provider,
            route: route(100),
            tools: &tools,
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spent,
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &cancel,
        }
        .run()
        .await
        .expect("the retried summary lands the compaction");

        assert_eq!(
            last_summary(&messages),
            Some("the summary"),
            "the second summary attempt's text is the marker"
        );
        assert_eq!(
            *provider.attempts.lock().expect("not poisoned"),
            4,
            "the step, the failed summary, the retried summary, then the step"
        );
        drop(tx);
        let mut retries = Vec::new();
        while let Some(event) = rx.recv().await {
            if let Event::Retry { attempt, delay } = event {
                retries.push((attempt, delay));
            }
        }
        assert_eq!(retries, [(1, Duration::ZERO)], "announced like a step's");
    }

    /// Streams a partial summary and then fails, like a dropped connection;
    /// the second summary attempt answers whole.
    struct DropsMidwaySummary {
        steps: Mutex<usize>,
        summaries: Mutex<usize>,
    }

    impl Provider for DropsMidwaySummary {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            async { Ok(Listing::default()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            request: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            if request.tools.is_empty() {
                let mut summaries = self.summaries.lock().expect("not poisoned");
                *summaries += 1;
                let first = *summaries == 1;
                return async move {
                    let events: Vec<Result<StreamEvent, BoxError>> = match first {
                        true => vec![
                            Ok(StreamEvent::TextDelta("half a summary".into())),
                            Err(Box::new(Temporary) as BoxError),
                        ],
                        false => vec![Ok(StreamEvent::TextDelta("the summary".into()))],
                    };
                    Ok(stream::iter(events).boxed())
                }
                .boxed();
            }
            let mut steps = self.steps.lock().expect("not poisoned");
            *steps += 1;
            let reply = match *steps {
                1 => vec![
                    StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#)),
                    StreamEvent::Usage(usage(9_000, 1_500)),
                ],
                _ => vec![StreamEvent::TextDelta("done".into())],
            };
            async move { Ok(stream::iter(reply.into_iter().map(Ok)).boxed()) }.boxed()
        }

        fn retry(&self, error: &BoxError) -> Option<Retry> {
            error.downcast_ref::<Temporary>().map(|_| Retry {
                // A retry that waits in a test would only slow it down.
                after: Some(Duration::ZERO),
            })
        }
    }

    /// A failure in the middle of the summary stream restarts it from
    /// scratch: only the complete second attempt becomes the summary.
    #[tokio::test]
    async fn a_mid_stream_summary_failure_restarts_the_summary() {
        let provider = DropsMidwaySummary {
            steps: Mutex::new(0),
            summaries: Mutex::new(0),
        };
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(64);
        let mut messages = vec![
            Message::System("sys".into()),
            Message::User("old question".into()),
            assistant(&"x".repeat(400)),
            Message::User("go".into()),
        ];
        let mut spent = spend();
        let mut last_usage = None;

        Turn {
            provider: &provider,
            route: route(100),
            tools: &tools,
            ctx: &ctx,
            messages: &mut messages,
            spend: &mut spent,
            last_usage: &mut last_usage,
            events: &tx,
            cancel: &cancel,
        }
        .run()
        .await
        .expect("the retried summary lands the compaction");

        assert_eq!(
            last_summary(&messages),
            Some("the summary"),
            "the complete second attempt's text, never the partial first"
        );
        assert_eq!(
            *provider.summaries.lock().expect("not poisoned"),
            2,
            "the summary streamed twice"
        );
        drop(tx);
        let mut retries = 0;
        while let Some(event) = rx.recv().await {
            retries += usize::from(matches!(event, Event::Retry { .. }));
        }
        assert_eq!(retries, 1, "one retry announced");
    }
}
