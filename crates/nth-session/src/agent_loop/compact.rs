//! One compaction: summarizing everything a request would not keep verbatim
//! into a [`Message::Compaction`] marker, spliced in before the verbatim
//! tail. Later requests start at the marker (see `stream::effective`).

use futures::StreamExt;
use nth_protocol::{BoxError, ContextOverflow, Event, Message, Request, StreamEvent, Usage};

use super::{Error, Stop, Turn, emit};
use crate::compaction::{self, SUMMARY_PROMPT};

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
    /// prompt, or a tail nothing can cut — so the turn gives up instead of
    /// asking for a summary before every step.
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

    /// Summarizes everything a request would not keep verbatim and splices
    /// the marker in before the tail. The messages before the marker stay
    /// in the history; only later requests start at it. `overflow` keeps
    /// the last user prompt and its exchange in the tail, so a prompt the
    /// provider refused runs again under the summary. `Ok(false)` when
    /// there was nothing to summarize: a caller that must shrink the
    /// conversation cannot, and one that only tried to keeps going.
    pub async fn compact(&mut self, overflow: bool) -> Result<bool, Error> {
        let compaction = self.route.compaction;
        let usable = compaction.usable(self.route.context_window, self.route.max_output);
        let mut tail = compaction::tail_start(self.messages, compaction.preserve(usable));
        // On an overflow the last real prompt and its exchange stay verbatim
        // so the turn can replay the prompt under the summary — but only
        // when there is anything else to summarize.
        if overflow
            && let Some(prompt) = self
                .messages
                .iter()
                .rposition(|message| matches!(message, Message::User(_)))
            && self.messages[..prompt]
                .iter()
                .any(|message| !matches!(message, Message::System(_)))
        {
            tail = tail.min(prompt);
        }
        // The head holds no conversation: everything fits the tail budget,
        // or only the prompt to replay is left. Summarize all of it, as
        // opencode does, and keep no tail.
        if !self.messages[..tail]
            .iter()
            .any(|message| !matches!(message, Message::System(_)))
        {
            tail = self.messages.len();
        }
        // The new marker goes after the last one, so requests start at this
        // summary rather than an older one.
        if let Some(last) = self
            .messages
            .iter()
            .rposition(|message| matches!(message, Message::Compaction { .. }))
        {
            tail = tail.max(last + 1);
        }
        // Only the system prompt: nothing to compact.
        if !self.messages[..tail]
            .iter()
            .any(|message| !matches!(message, Message::System(_)))
        {
            return Ok(false);
        }
        let head = compaction::serialize_head(&self.messages[..tail]);
        let summary = self
            .summarize(format!("{SUMMARY_PROMPT}\n\n{head}"))
            .await?;
        self.messages.insert(
            tail,
            Message::Compaction {
                summary: summary.clone(),
            },
        );
        emit(self.events, Event::Compacted { summary }).await;
        Ok(true)
    }

    /// The summary request: one user message, no tools, streamed to the
    /// end. Opening it is retried as a step's is; its usage is counted like
    /// a step's, but its text is not streamed as an answer — the front-end
    /// hears of it as [`Event::Compacted`].
    async fn summarize(&mut self, prompt: String) -> Result<String, Error> {
        let messages = [Message::User(prompt)];
        let request = Request {
            model: self.route.model,
            session_id: self.route.session_id,
            parent_session_id: self.route.parent_session_id,
            effort: self.route.effort,
            messages: &messages,
            tools: &[],
        };
        let mut stream = match self.open_stream(request).await {
            Ok(stream) => stream,
            Err(Stop::Cancelled(_)) => return Err(Error::Interrupted),
            Err(Stop::Failed(error)) => return Err(self.request_failed(error)),
        };
        let (events, cancel) = (self.events, self.cancel);
        let mut summary = String::new();
        loop {
            let event = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Error::Interrupted),
                event = stream.next() => event,
            };
            let Some(event) = event else { break };
            match event.map_err(|error| self.request_failed(error))? {
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
        if summary.trim().is_empty() {
            return Err(Error::EmptyReply);
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
        compaction::WHAT_DID_WE_DO,
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

    #[tokio::test]
    async fn a_compaction_splices_the_marker_before_the_tail() {
        let provider = Scripted::new(vec![vec![
            StreamEvent::TextDelta("# Objective\nship it".into()),
            StreamEvent::Usage(usage(100, 10)),
        ]]);
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel(16);
        // An old turn too big for the budget, then a recent one kept whole.
        let mut messages = vec![
            Message::System("sys".into()),
            Message::User("first question".into()),
            assistant(&"x".repeat(400)),
            Message::User("second question".into()),
            assistant("second answer"),
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
        .compact(false)
        .await
        .expect("compacts");

        assert_eq!(
            messages[3],
            Message::Compaction {
                summary: "# Objective\nship it".into()
            },
            "the marker sits between the head and the tail"
        );
        assert_eq!(
            messages[4..],
            [
                Message::User("second question".into()),
                assistant("second answer")
            ]
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
        assert!(prompt.starts_with(SUMMARY_PROMPT), "the template opens it");
        assert!(prompt.contains("[User]: first question"), "{prompt}");
        assert!(
            prompt.contains(&format!("[Assistant]: {}", "x".repeat(400))),
            "the serialized answer"
        );
        assert!(
            !prompt.contains("second question"),
            "the verbatim tail is not serialized"
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

        assert_eq!(
            messages[3],
            Message::Compaction {
                summary: "the summary".into()
            }
        );
        assert!(matches!(messages[4], Message::User(_)), "the tail stays");
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
                "{SUMMARY_PROMPT}\n\n[User]: old question\n[Assistant]: {}",
                "x".repeat(400)
            ))],
            "the summary request carries the template and the serialized head"
        );
        assert_eq!(
            sent[2][..3],
            [
                Message::System("sys".into()),
                Message::User(WHAT_DID_WE_DO.into()),
                assistant("the summary"),
            ],
            "the step after starts at the summary"
        );
        assert_eq!(
            sent[2][3],
            Message::User("go".into()),
            "then the verbatim tail"
        );
        drop(tx);
        let mut compacted = 0;
        while let Some(event) = rx.recv().await {
            compacted += usize::from(matches!(event, Event::Compacted { .. }));
        }
        assert_eq!(compacted, 1);
    }

    /// Fails its first request with a context overflow, as a wire will once
    /// it recognizes its provider's overflow error; answers the rest.
    struct OverflowThen {
        replies: Mutex<Vec<Vec<StreamEvent>>>,
        sent: Mutex<Vec<Vec<Message>>>,
    }

    impl Provider for OverflowThen {
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
            if sent.len() == 1 {
                return async { Err(Box::new(ContextOverflow) as BoxError) }.boxed();
            }
            let reply = self.replies.lock().expect("not poisoned").remove(0);
            async move { Ok(stream::iter(reply.into_iter().map(Ok)).boxed()) }.boxed()
        }
    }

    #[tokio::test]
    async fn an_overflowing_step_compacts_and_replays_the_prompt() {
        let provider = OverflowThen {
            replies: Mutex::new(vec![
                vec![StreamEvent::TextDelta("the summary".into())],
                vec![StreamEvent::TextDelta("done".into())],
            ]),
            sent: Mutex::new(Vec::new()),
        };
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

        assert!(matches!(messages[3], Message::Compaction { .. }));
        assert_eq!(
            messages[4..],
            [Message::User("the failed prompt".into()), assistant("done")],
            "the prompt is replayed under the summary"
        );

        let sent = provider.sent.lock().expect("not poisoned").clone();
        assert_eq!(sent.len(), 3, "the overflow, the summary, the replay");
        let [Message::User(prompt)] = &sent[1][..] else {
            panic!("one user message: {:?}", sent[1]);
        };
        assert!(prompt.contains("old question"), "{prompt}");
        assert!(
            !prompt.contains("the failed prompt"),
            "the replayed prompt is not summarized"
        );
        assert_eq!(
            sent[2],
            [
                Message::System("sys".into()),
                Message::User(WHAT_DID_WE_DO.into()),
                assistant("the summary"),
                Message::User("the failed prompt".into()),
            ]
        );
    }

    #[tokio::test]
    async fn a_second_compaction_lands_after_the_first_and_requests_start_there() {
        let provider = Scripted::new(vec![
            vec![StreamEvent::TextDelta("second summary".into())],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(64);
        let mut messages = vec![
            Message::System("sys".into()),
            Message::Compaction {
                summary: "first summary".into(),
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

        turn.compact(false).await.expect("compacts");
        assert!(turn.stream_step(false, &[], &mut None).await.is_ok());

        assert_eq!(
            messages[4],
            Message::Compaction {
                summary: "second summary".into()
            },
            "the new marker follows the old one"
        );
        let sent = provider.sent.lock().expect("not poisoned").clone();
        assert_eq!(
            sent[1][..3],
            [
                Message::System("sys".into()),
                Message::User(WHAT_DID_WE_DO.into()),
                assistant("second summary"),
            ],
            "the request starts at the newest summary"
        );
        let [Message::User(prompt)] = &sent[0][..] else {
            panic!("one user message: {:?}", sent[0]);
        };
        assert!(
            prompt.contains("[Summary]: first summary"),
            "the earlier summary is a line of the head: {prompt}"
        );
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
        /// A retryable provider failure that is not an overflow.
        #[derive(Debug, thiserror::Error)]
        #[error("temporary failure")]
        struct Temporary;

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
            messages[3],
            Message::Compaction {
                summary: "the summary".into()
            },
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
}
