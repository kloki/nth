//! Anthropic messages over SSE, for the models the catalogue says need it
//! (Claude on OpenCode Zen, MiniMax on Go).

mod event;
mod wire;

use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

use futures::{Stream, StreamExt, stream::BoxStream};
use nth_protocol::{BoxError, Request, Retry, StreamEvent, ToolCall, Usage};

use crate::{
    catalog,
    http::{STREAM_IDLE_TIMEOUT, USER_AGENT, retry_after, retryable, transient},
};

/// The API version the request and the stream are shaped for.
const VERSION: &str = "2023-06-01";
/// The reply's limit when the catalogue does not know the model's. Also
/// the most asked for, as opencode does: the endpoint's rate limits count
/// what a request may produce, not what it does.
const MAX_TOKENS: u64 = 32_000;

/// The `max_tokens` to ask for, from the model's output limit if known.
fn max_tokens(known: Option<&catalog::Model>) -> u64 {
    let output = known.and_then(|m| m.limit.as_ref()?.output);
    output.map_or(MAX_TOKENS, |output| output.min(MAX_TOKENS))
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{status}: {message}")]
    Status {
        status: reqwest::StatusCode,
        /// The error body's message, or the whole body when it has none.
        message: String,
        /// The server's `Retry-After`, when it sent one.
        retry_after: Option<Duration>,
    },
    #[error("bad stream event: {source} in {line:?}")]
    Parse {
        source: serde_json::Error,
        /// The start of the offending line; see `crate::event::EXCERPT_CHARS`.
        line: String,
    },
    /// An `error` event, sent after the 200 status.
    #[error("provider error {kind}: {message}")]
    Provider { kind: String, message: String },
    #[error("stream ended before the response was complete")]
    Incomplete,
    #[error("stream stalled: nothing arrived for {}s", STREAM_IDLE_TIMEOUT.as_secs())]
    Stalled,
    #[error("response hit the output token limit")]
    Truncated,
}

/// One endpoint's messages route.
pub(crate) struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Client {
    pub(crate) fn new(http: reqwest::Client, base_url: String, api_key: String) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
        }
    }

    /// `known` is the catalogue's entry for the model, if it has one: its
    /// output limit bounds `max_tokens`, which is required here, unlike in
    /// chat completions.
    pub(crate) async fn stream(
        &self,
        request: Request<'_>,
        known: Option<&catalog::Model>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, BoxError>>, Error> {
        let response = self
            .http
            .post(format!("{}/messages", self.base_url))
            // OpenCode Zen refuses a bearer token here, as Anthropic does.
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", VERSION)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header("x-opencode-session", request.session_id)
            .json(&wire::body(
                request.model,
                request.effort,
                max_tokens(known),
                request.messages,
                request.tools,
            ))
            .send()
            .await?;
        let response = success(response).await?;

        let bytes = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(Error::from))
            .boxed();
        Ok(events(bytes).map(|e| e.map_err(BoxError::from)).boxed())
    }
}

/// Turns a non-2xx response into an error that carries the server's
/// message and its `Retry-After`, if any.
async fn success(response: reqwest::Response) -> Result<reqwest::Response, Error> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    let message = match serde_json::from_str::<event::ErrorResponse>(&body) {
        Ok(error) => error.error.message,
        Err(_) => body,
    };
    Err(Error::Status {
        status,
        message,
        retry_after,
    })
}

/// Whether `error`, one a messages `Client` produced, may be retried: the
/// same rules as chat completions'.
pub(crate) fn retry(error: &BoxError) -> Option<Retry> {
    let error = error.downcast_ref::<Error>()?;
    match error {
        Error::Status {
            status,
            retry_after,
            ..
        } if transient(*status) => Some(Retry {
            after: *retry_after,
        }),
        // A rate limit, an overload or a server failure reported inside
        // the stream; the kinds stand for 429, 529 and 500.
        Error::Provider { kind, .. }
            if matches!(
                kind.as_str(),
                "rate_limit_error" | "overloaded_error" | "api_error"
            ) =>
        {
            Some(Retry { after: None })
        }
        Error::Http(e) if retryable(e) => Some(Retry { after: None }),
        Error::Incomplete | Error::Stalled => Some(Retry { after: None }),
        _ => None,
    }
}

struct State<S> {
    bytes: S,
    parser: event::Parser,
    pending: VecDeque<Result<StreamEvent, Error>>,
    /// Tool calls by the index of their content block.
    calls: BTreeMap<usize, ToolCall>,
    usage: event::Usage,
    stop_reason: Option<String>,
    finished: bool,
}

/// Text and reasoning go out as they arrive. Tool calls are assembled from
/// their fragments and go out once the stream ends, when they are complete.
fn events<S>(bytes: S) -> impl Stream<Item = Result<StreamEvent, Error>>
where
    S: Stream<Item = Result<bytes::Bytes, Error>> + Unpin,
{
    let state = State {
        bytes,
        parser: event::parser(),
        pending: VecDeque::new(),
        calls: BTreeMap::new(),
        usage: event::Usage::default(),
        stop_reason: None,
        finished: false,
    };
    futures::stream::unfold(state, |mut s| async move {
        loop {
            if let Some(event) = s.pending.pop_front() {
                return Some((event, s));
            }
            if s.finished {
                return None;
            }
            let next = tokio::time::timeout(STREAM_IDLE_TIMEOUT, s.bytes.next()).await;
            let (parsed, ended) = match next {
                Ok(Some(Ok(chunk))) => (s.parser.push(&chunk), false),
                // The reply is complete once its stop reason arrived; what
                // is lost with the connection after that is at most the
                // `message_stop` event, not worth failing the turn over.
                Ok(Some(Err(_))) | Err(_) if s.stop_reason.is_some() => (Ok(Vec::new()), true),
                Ok(Some(Err(e))) => (Err(e), false),
                Err(_elapsed) => (Err(Error::Stalled), false),
                // A remainder that does not parse is a line the connection
                // cut short, which the end-of-stream rules below describe
                // better than a parse error would.
                Ok(None) => (Ok(s.parser.finish().unwrap_or_default()), true),
            };
            let parsed = match parsed {
                Ok(parsed) => parsed,
                Err(e) => {
                    s.finished = true;
                    return Some((Err(e), s));
                }
            };
            for event in parsed {
                if s.apply(event) {
                    s.done();
                    break;
                }
            }
            if ended && !s.finished {
                if s.stop_reason.is_some() {
                    s.done();
                } else {
                    s.finished = true;
                    s.pending.push_back(Err(Error::Incomplete));
                }
            }
        }
    })
}

impl<S> State<S> {
    /// Takes in one event; true once the message is over.
    fn apply(&mut self, event: event::Event) -> bool {
        match event {
            event::Event::MessageStart { message } => {
                self.usage.update(message.usage.unwrap_or_default());
            }
            event::Event::ContentBlockStart {
                index,
                content_block: event::Block::ToolUse { id, name },
            } => {
                self.calls.insert(
                    index,
                    ToolCall {
                        id,
                        name,
                        arguments: String::new(),
                    },
                );
            }
            event::Event::ContentBlockDelta { index, delta } => match delta {
                event::Delta::Text { text } if !text.is_empty() => {
                    self.pending.push_back(Ok(StreamEvent::TextDelta(text)));
                }
                event::Delta::Thinking { thinking } if !thinking.is_empty() => {
                    self.pending
                        .push_back(Ok(StreamEvent::ReasoningDelta(thinking)));
                }
                event::Delta::InputJson { partial_json } => {
                    if let Some(call) = self.calls.get_mut(&index) {
                        call.arguments.push_str(&partial_json);
                    }
                }
                _ => {}
            },
            event::Event::MessageDelta { delta, usage } => {
                self.stop_reason = delta.stop_reason;
                self.usage.update(usage.unwrap_or_default());
                self.pending.push_back(Ok(StreamEvent::Usage(Usage {
                    input: self.usage.input(),
                    output: self.usage.output(),
                })));
            }
            event::Event::MessageStop => return true,
            event::Event::ContentBlockStart { .. }
            | event::Event::Error { .. }
            | event::Event::Other => {}
        }
        false
    }

    fn done(&mut self) {
        self.finished = true;
        // Tool calls cut off at the token limit have partial JSON input and
        // must not run.
        if self.stop_reason.as_deref() == Some("max_tokens") {
            self.pending.push_back(Err(Error::Truncated));
            return;
        }
        let calls = std::mem::take(&mut self.calls);
        self.pending.extend(calls.into_values().map(|mut call| {
            // A call without parameters streams no input at all.
            if call.arguments.is_empty() {
                call.arguments = "{}".into();
            }
            Ok(StreamEvent::ToolCall(call))
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{chunks, runs_no_tools, until};

    /// Recorded from OpenCode Zen's claude-haiku-4-5, thinking enabled.
    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/zen_messages.sse");

    async fn replay(chunk_size: usize) -> Vec<StreamEvent> {
        replay_bytes(FIXTURE, chunk_size)
            .await
            .into_iter()
            .map(|e| e.expect("fixture is valid"))
            .collect()
    }

    async fn replay_bytes(input: &[u8], chunk_size: usize) -> Vec<Result<StreamEvent, Error>> {
        events(futures::stream::iter(chunks(input, chunk_size)))
            .collect()
            .await
    }

    fn fixture_until(marker: &str) -> &'static [u8] {
        until(FIXTURE, marker)
    }

    #[tokio::test]
    async fn assembles_text_reasoning_usage_and_tool_calls() {
        let events = replay(FIXTURE.len()).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::ReasoningDelta("Read both.".into()),
                StreamEvent::TextDelta("Reading wörld".into()),
                StreamEvent::TextDelta(" files.".into()),
                StreamEvent::Usage(Usage {
                    input: 614,
                    output: 120,
                }),
                StreamEvent::ToolCall(ToolCall {
                    id: "toolu_01A".into(),
                    name: "read".into(),
                    arguments: r#"{"filePath": "Cargo.toml"}"#.into(),
                }),
                StreamEvent::ToolCall(ToolCall {
                    id: "toolu_01B".into(),
                    name: "glob".into(),
                    arguments: "{}".into(),
                }),
            ]
        );
    }

    #[tokio::test]
    async fn chunk_boundaries_do_not_matter() {
        let whole = replay(FIXTURE.len()).await;
        for size in 1..16 {
            assert_eq!(replay(size).await, whole, "chunk size {size}");
        }
    }

    #[tokio::test]
    async fn missing_message_stop_after_the_stop_reason_is_fine() {
        let events = replay_bytes(fixture_until("event: message_stop"), FIXTURE.len()).await;
        let events: Vec<_> = events.into_iter().map(|e| e.expect("valid")).collect();
        assert_eq!(events, replay(FIXTURE.len()).await);
    }

    #[tokio::test]
    async fn body_error_after_the_stop_reason_keeps_the_reply() {
        let mut input = chunks(fixture_until("event: message_stop"), FIXTURE.len());
        input.push(Err(Error::Incomplete));
        let events: Vec<_> = events(futures::stream::iter(input)).collect().await;
        let events: Vec<_> = events.into_iter().map(|e| e.expect("valid")).collect();
        assert_eq!(events, replay(FIXTURE.len()).await);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_stream_is_a_retryable_error() {
        let head = fixture_until("event: message_delta");
        let bytes =
            futures::stream::iter(chunks(head, head.len())).chain(futures::stream::pending());
        let events: Vec<_> = events(bytes).collect().await;
        assert!(
            matches!(events.last(), Some(Err(Error::Stalled))),
            "{events:?}"
        );
        assert!(runs_no_tools(&events));
        let stalled: BoxError = Box::new(Error::Stalled);
        assert_eq!(retry(&stalled), Some(Retry { after: None }));
    }

    #[tokio::test]
    async fn cut_off_stream_is_an_error_and_runs_no_tools() {
        let events = replay_bytes(fixture_until("event: message_delta"), FIXTURE.len()).await;
        assert!(matches!(events.last(), Some(Err(Error::Incomplete))));
        assert!(runs_no_tools(&events));
    }

    #[tokio::test]
    async fn max_tokens_stop_is_an_error_and_runs_no_tools() {
        let text = std::str::from_utf8(FIXTURE).expect("utf-8 fixture");
        let input = text.replace(
            "\"stop_reason\":\"tool_use\"",
            "\"stop_reason\":\"max_tokens\"",
        );
        let events = replay_bytes(input.as_bytes(), input.len()).await;
        assert!(matches!(events.last(), Some(Err(Error::Truncated))));
        assert!(runs_no_tools(&events));
    }

    #[tokio::test]
    async fn an_error_event_ends_the_stream() {
        let input = [
            fixture_until("event: content_block_start"),
            b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        ]
        .concat();
        let events = replay_bytes(&input, input.len()).await;
        let Some(Err(error)) = events.last() else {
            panic!("expected an error, got {events:?}");
        };
        assert!(matches!(error, Error::Provider { kind, .. } if kind == "overloaded_error"));
    }

    #[test]
    fn max_tokens_is_the_output_limit_capped() {
        let model = |output| catalog::Model {
            name: None,
            reasoning: None,
            limit: Some(catalog::Limit {
                context: None,
                output,
            }),
            provider: None,
        };
        assert_eq!(max_tokens(None), MAX_TOKENS);
        assert_eq!(max_tokens(Some(&model(None))), MAX_TOKENS);
        assert_eq!(max_tokens(Some(&model(Some(8_192)))), 8_192);
        assert_eq!(max_tokens(Some(&model(Some(131_072)))), MAX_TOKENS);
    }

    #[test]
    fn only_transient_errors_are_retryable() {
        let boxed = |error: Error| -> BoxError { Box::new(error) };
        let status = |status, retry_after| {
            boxed(Error::Status {
                status,
                message: String::new(),
                retry_after,
            })
        };
        let in_stream = |kind: &str| {
            boxed(Error::Provider {
                kind: kind.into(),
                message: String::new(),
            })
        };

        assert_eq!(
            retry(&status(
                reqwest::StatusCode::TOO_MANY_REQUESTS,
                Some(Duration::from_secs(7))
            )),
            Some(Retry {
                after: Some(Duration::from_secs(7))
            })
        );
        let overloaded = reqwest::StatusCode::from_u16(529).expect("valid status");
        assert_eq!(
            retry(&status(overloaded, None)),
            Some(Retry { after: None })
        );
        assert_eq!(retry(&status(reqwest::StatusCode::BAD_REQUEST, None)), None);
        assert_eq!(
            retry(&status(reqwest::StatusCode::UNAUTHORIZED, None)),
            None
        );

        for kind in ["overloaded_error", "rate_limit_error", "api_error"] {
            assert_eq!(
                retry(&in_stream(kind)),
                Some(Retry { after: None }),
                "{kind}"
            );
        }
        assert_eq!(retry(&in_stream("invalid_request_error")), None);

        assert_eq!(
            retry(&boxed(Error::Incomplete)),
            Some(Retry { after: None })
        );
        assert_eq!(retry(&boxed(Error::Truncated)), None);
        let other: BoxError = Box::new(crate::chat_completions::Error::Incomplete);
        assert_eq!(retry(&other), None, "another protocol's error");
    }
}
