//! One compaction: summarizing the conversation since the last
//! [`Message::Compaction`] into a new one appended at the end, which keeps
//! the newest messages beside the summary as text. Later requests start at
//! it (see [`effective`](super::effective)).

use futures::StreamExt;
use nth_protocol::{
    AssistantMessage, BoxError, ContextOverflow, Event, Message, Request, StreamEvent, Usage,
};

use crate::{
    agent_loop::{Error, Stop, Turn, emit},
    compaction::{self, SHRINK_STEPS},
};

impl Turn<'_> {
    /// Whether the next step should summarize instead of asking the model:
    /// the session has automatic compaction on, knows the model's window
    /// with room left under the reserved buffer, and the last finished
    /// reply's usage reached the usable part of it.
    pub(crate) fn compaction_due(&self, last_usage: Option<Usage>) -> bool {
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
    pub(crate) fn still_over_the_window(&self) -> bool {
        let Some(usable) = self
            .route
            .compaction
            .usable(self.route.context_window, self.route.max_output)
        else {
            return false;
        };
        usable > 0 && compaction::estimate(&compaction::effective(self.messages)) >= usable
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
        let last = compaction::last_marker(self.messages).map(|at| (at, &self.messages[at]));
        let (since, previous, previous_recent) = match last {
            Some((at, Message::Compaction { summary, recent })) => {
                (at + 1, Some(summary.clone()), recent.clone())
            }
            _ => (0, None, String::new()),
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
        self.publish_preview("ready", false, None);
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
        let request = self.route.request(&messages, &[]);
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
    use std::time::Duration;

    use nth_protocol::{AssistantMessage, StreamEvent, Tool, ToolContext, Usage};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::{
        Compaction, Route,
        agent_loop::tests::{Echo, Failure, ROUTE, Reply, Scripted, call, spend},
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

    /// The summary requests `provider` was sent: the ones offering no tools.
    fn summaries(provider: &Scripted) -> usize {
        let tools = provider.tools.lock().expect("not poisoned");
        tools.iter().filter(|offered| offered.is_empty()).count()
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
        let mut preview = None;
        while let Some(event) = rx.recv().await {
            match event {
                Event::ContextUpdated(value) => preview = Some(value),
                event => events.push(event),
            }
        }
        let preview: crate::context_preview::Preview =
            serde_json::from_value((*preview.expect("checkpoint published")).clone()).unwrap();
        assert_eq!(preview.phase, "ready");
        assert_eq!(preview.last_usage, None);
        assert_eq!(
            preview
                .entries
                .iter()
                .filter(|entry| entry.checkpoint)
                .count(),
            1
        );
        assert_eq!(
            preview
                .entries
                .iter()
                .map(|entry| &entry.message)
                .collect::<Vec<_>>(),
            crate::compaction::effective(&messages)
                .iter()
                .collect::<Vec<_>>()
        );
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

    fn overflowing_conversation() -> Vec<Message> {
        vec![
            Message::System("sys".into()),
            Message::User("old question".into()),
            assistant(&"x".repeat(400)),
            Message::User("the failed prompt".into()),
        ]
    }

    #[tokio::test]
    async fn a_second_overflow_in_the_same_step_gives_up() {
        let provider = Scripted::script(vec![
            Reply::Refused(Failure::Overflow),
            Reply::Events(vec![StreamEvent::TextDelta("the summary".into())]),
            Reply::Refused(Failure::Overflow),
        ]);
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
        assert_eq!(provider.requests(), 3);
    }

    #[tokio::test]
    async fn an_overflow_in_a_later_step_compacts_again() {
        let provider = Scripted::script(vec![
            Reply::Refused(Failure::Overflow),
            Reply::Events(vec![StreamEvent::TextDelta("first summary".into())]),
            Reply::Events(vec![StreamEvent::ToolCall(call(
                "1",
                "echo",
                r#"{"say":"hi"}"#,
            ))]),
            Reply::Refused(Failure::Overflow),
            Reply::Events(vec![StreamEvent::TextDelta("second summary".into())]),
            Reply::Events(vec![StreamEvent::TextDelta("done".into())]),
        ]);
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
    async fn a_refused_summary_request_is_sent_again_without_its_oldest_exchanges() {
        // The summary request is refused once, then answered.
        let provider = Scripted::script(vec![
            Reply::Refused(Failure::Overflow),
            Reply::Events(vec![StreamEvent::TextDelta("the summary".into())]),
        ]);
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

    /// Compacting cannot shrink a conversation whose system prompt alone
    /// fills the window: the turn must stop at once rather than ask for a
    /// summary before every one of its steps.
    #[tokio::test]
    async fn a_floor_over_the_window_stops_the_turn_instead_of_compacting_every_step() {
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo)];
        let ctx = ToolContext::new(".".into());
        let cancel = CancellationToken::new();
        let (tx, _rx) = mpsc::channel(64);
        // Some 15k estimated tokens, well over the 10k − 2k usable window.
        let mut messages = vec![
            Message::System("s".repeat(60_000)),
            Message::User("go".into()),
        ];
        // The step reports the request's own size as used; the summary
        // cannot shrink the system prompt, so a turn that compacted before
        // every step would ask for more than these two.
        let provider = Scripted::script(vec![
            Reply::Events(vec![
                StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi-1"}"#)),
                StreamEvent::Usage(usage(compaction::estimate(&messages), 10)),
            ]),
            Reply::Events(vec![StreamEvent::TextDelta(
                "# Objective\nstill over".into(),
            )]),
        ]);
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
            summaries(&provider),
            1,
            "one summary, not one before each of the four steps"
        );
    }

    /// A window that is all reserved buffer has no usable room: no usage,
    /// however large, can be helped by compacting, so the trigger never
    /// fires.
    #[tokio::test]
    async fn a_window_with_no_usable_room_never_compacts() {
        // Each step's usage fills the window; a different call each step,
        // so the doom-loop guard does not end the turn first.
        let full = usage(1_000, 10);
        let provider = Scripted::script(vec![
            Reply::Events(vec![
                StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi-1"}"#)),
                StreamEvent::Usage(full),
            ]),
            Reply::Events(vec![
                StreamEvent::ToolCall(call("2", "echo", r#"{"say":"hi-2"}"#)),
                StreamEvent::Usage(full),
            ]),
        ]);
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
            summaries(&provider),
            0,
            "nothing fires with no room to compact into"
        );
        assert_eq!(provider.requests(), 2, "just the two steps");
    }

    /// A transient failure on the summary request is retried exactly as a
    /// step's opening is, so the compaction still lands.
    #[tokio::test]
    async fn a_retryable_summary_failure_is_retried() {
        // The step, then the summary, which fails; the retried summary
        // answers.
        let provider = Scripted::script(vec![
            Reply::Events(vec![
                StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#)),
                StreamEvent::Usage(usage(9_000, 1_500)),
            ]),
            Reply::Refused(Failure::Temporary),
            Reply::Events(vec![StreamEvent::TextDelta("the summary".into())]),
            Reply::Events(vec![StreamEvent::TextDelta("done".into())]),
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
        .expect("the retried summary lands the compaction");

        assert_eq!(
            last_summary(&messages),
            Some("the summary"),
            "the second summary attempt's text is the marker"
        );
        assert_eq!(
            provider.requests(),
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

    /// A failure in the middle of the summary stream restarts it from
    /// scratch: only the complete second attempt becomes the summary.
    #[tokio::test]
    async fn a_mid_stream_summary_failure_restarts_the_summary() {
        // Streams a partial summary and then fails, like a dropped
        // connection; the second summary attempt answers whole.
        let provider = Scripted::script(vec![
            Reply::Events(vec![
                StreamEvent::ToolCall(call("1", "echo", r#"{"say":"hi"}"#)),
                StreamEvent::Usage(usage(9_000, 1_500)),
            ]),
            Reply::Breaks(vec![StreamEvent::TextDelta("half a summary".into())]),
            Reply::Events(vec![StreamEvent::TextDelta("the summary".into())]),
            Reply::Events(vec![StreamEvent::TextDelta("done".into())]),
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
        .expect("the retried summary lands the compaction");

        assert_eq!(
            last_summary(&messages),
            Some("the summary"),
            "the complete second attempt's text, never the partial first"
        );
        assert_eq!(summaries(&provider), 2, "the summary streamed twice");
        drop(tx);
        let mut retries = 0;
        while let Some(event) = rx.recv().await {
            retries += usize::from(matches!(event, Event::Retry { .. }));
        }
        assert_eq!(retries, 1, "one retry announced");
    }
}
