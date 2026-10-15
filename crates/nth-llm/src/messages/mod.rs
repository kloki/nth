//! Anthropic messages over SSE, for the models the catalogue says need it
//! (Claude on OpenCode Zen, MiniMax on Go).

mod event;
mod wire;

use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

use futures::{Stream, StreamExt, stream::BoxStream};
use nth_protocol::{BoxError, ContextOverflow, Request, Retry, StreamEvent, ToolCall, Usage};

use crate::{
    auth::Auth,
    catalog,
    http::{self, STREAM_IDLE_TIMEOUT, USER_AGENT, retry_after, retryable, transient},
};

/// The API version the request and the stream are shaped for.
const VERSION: &str = "2023-06-01";
/// The reply's limit when the catalogue does not know the model's. Also
/// the most asked for, as opencode does: the endpoint's rate limits count
/// what a request may produce, not what it does.
const MAX_TOKENS: u64 = 32_000;

/// How the model is told to think: adaptively when it takes effort levels
/// and no budget, else within a budget, with the level too when it takes
/// one.
fn thinking(known: Option<&catalog::Model>) -> wire::Thinking {
    let Some(known) = known else {
        return wire::Thinking::UNKNOWN;
    };
    match (known.takes_effort(), known.budget()) {
        (true, None) => wire::Thinking::Adaptive,
        (effort, budget) => {
            let (min, max) = budget.unwrap_or_default();
            wire::Thinking::Budget { min, max, effort }
        }
    }
}

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
}

impl Client {
    pub(crate) fn new(http: reqwest::Client, base_url: String) -> Self {
        Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
        }
    }

    /// `known` is the catalogue's entry for the model, if it has one: its
    /// output limit bounds `max_tokens`, which is required here, unlike in
    /// chat completions, and its reasoning options say how it thinks.
    pub(crate) async fn stream(
        &self,
        request: Request<'_>,
        known: Option<&catalog::Model>,
        auth: &Auth,
    ) -> Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError> {
        let signed = auth.api_key(
            self.http.post(format!("{}/messages", self.base_url)),
            Some(request.session_id),
        );
        let response = http::session(signed, &request)
            .header("anthropic-version", VERSION)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .json(&wire::body(
                request.model,
                request.effort,
                thinking(known),
                max_tokens(known),
                request.messages,
                request.tools,
            ))
            .send()
            .await
            .map_err(Error::from)?;
        let response = success(response).await?;

        let bytes = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(Error::from))
            .boxed();
        Ok(events(bytes).map(|e| e.map_err(BoxError::from)).boxed())
    }
}

/// Turns a non-2xx response into the stream's error: the shared
/// [`ContextOverflow`] marker when the body is a refusal as too long, so
/// the loop compacts and replays, else the status error it always was.
async fn success(response: reqwest::Response) -> Result<reqwest::Response, BoxError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    Err(status_error(status, body, retry_after))
}

/// A failed response as its stream's error; apart from `success` so the
/// classification is testable without an HTTP response in hand.
fn status_error(
    status: reqwest::StatusCode,
    body: String,
    retry_after: Option<Duration>,
) -> BoxError {
    if http::context_overflow(status, &body) {
        return Box::new(ContextOverflow);
    }
    let message = match serde_json::from_str::<event::ErrorResponse>(&body) {
        Ok(error) => error.error.message,
        Err(_) => body,
    };
    Box::new(Error::Status {
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
                    cache_read: self.usage.cache_read_input_tokens,
                    cache_write: self.usage.cache_creation_input_tokens,
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
    use crate::testing::{chunks, runs_no_tools, serve_status, until};

    /// Recorded from OpenCode Zen's claude-haiku-4-5, thinking enabled.
    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/zen_messages.sse");

    /// Anthropic's refusal, as its API sends it.
    const OVERFLOW: &[u8] = br#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 213866 tokens > 200000 maximum"}}"#;

    /// A request with nothing in it: the tests here only care which error a
    /// failed response becomes.
    fn request() -> Request<'static> {
        Request {
            model: "test-model",
            session_id: "ses_test",
            parent_session_id: None,
            effort: Default::default(),
            messages: &[],
            tools: &[],
        }
    }

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
                    cache_read: Some(600),
                    cache_write: Some(0),
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
    fn the_catalogue_says_how_a_model_thinks() {
        let catalog: catalog::Catalog =
            serde_json::from_str(include_str!("../../tests/fixtures/models_dev.json"))
                .expect("valid fixture");
        let other = &catalog["other"].models;
        assert_eq!(thinking(other.get("claude-x")), wire::Thinking::Adaptive);
        assert_eq!(
            thinking(other.get("claude-old")),
            wire::Thinking::Budget {
                min: Some(1_024),
                max: None,
                effort: false
            }
        );
        assert_eq!(thinking(None), wire::Thinking::UNKNOWN);
    }

    #[test]
    fn max_tokens_is_the_output_limit_capped() {
        let model = |output| catalog::Model {
            limit: Some(catalog::Limit {
                context: None,
                output,
            }),
            ..catalog::Model::default()
        };
        assert_eq!(max_tokens(None), MAX_TOKENS);
        assert_eq!(max_tokens(Some(&model(None))), MAX_TOKENS);
        assert_eq!(max_tokens(Some(&model(Some(8_192)))), 8_192);
        assert_eq!(max_tokens(Some(&model(Some(131_072)))), MAX_TOKENS);
    }

    #[tokio::test]
    async fn an_overflow_refusal_becomes_a_context_overflow() {
        let (url, _) = serve_status(400, OVERFLOW).await;
        let client = Client::new(reqwest::Client::new(), url);
        let error = client
            .stream(request(), None, &Auth::Key("key".into()))
            .await
            .err()
            .expect("refused");
        assert!(error.downcast_ref::<ContextOverflow>().is_some(), "{error}");
        assert_eq!(retry(&error), None, "compacting, not retrying, is the fix");
    }

    /// The refusal phrased in a nested field, and as plain text: the
    /// classification reads the whole body, not one field of it.
    const NESTED_OVERFLOW: &[u8] = br#"{"error":{"message":"Request failed","detail":{"reason":"maximum context length exceeded"}}}"#;
    const PLAIN_OVERFLOW: &[u8] = b"Input is too long for requested model";
    /// An ordinary refusal that says nothing about a limit.
    const ORDINARY: &[u8] =
        br#"{"error":{"type":"invalid_request_error","message":"invalid model"}}"#;

    #[tokio::test]
    async fn any_overflow_phrasing_on_a_400_413_or_422_becomes_a_context_overflow() {
        for status in [400, 413, 422] {
            for body in [NESTED_OVERFLOW, PLAIN_OVERFLOW] {
                let (url, _) = serve_status(status, body).await;
                let client = Client::new(reqwest::Client::new(), url);
                let error = client
                    .stream(request(), None, &Auth::Key("key".into()))
                    .await
                    .err()
                    .expect("refused");
                assert!(
                    error.downcast_ref::<ContextOverflow>().is_some(),
                    "{status} with {}: {error}",
                    String::from_utf8_lossy(body)
                );
                assert_eq!(retry(&error), None, "compacting, not retrying, is the fix");
            }
        }
    }

    #[tokio::test]
    async fn an_overflow_body_on_a_transient_status_stays_a_status_error() {
        for status in [429, 500] {
            let (url, _) = serve_status(status, OVERFLOW).await;
            let client = Client::new(reqwest::Client::new(), url);
            let error = client
                .stream(request(), None, &Auth::Key("key".into()))
                .await
                .err()
                .expect("refused");
            assert!(
                error.downcast_ref::<ContextOverflow>().is_none(),
                "{status}: {error}"
            );
            let expected = reqwest::StatusCode::from_u16(status).expect("a status");
            assert!(
                matches!(
                    error.downcast_ref::<Error>(),
                    Some(Error::Status { status, .. }) if *status == expected
                ),
                "{error}"
            );
            assert!(retry(&error).is_some(), "{status} is worth retrying");
        }
    }

    #[tokio::test]
    async fn an_ordinary_400_stays_a_status_error() {
        let (url, _) = serve_status(400, ORDINARY).await;
        let client = Client::new(reqwest::Client::new(), url);
        let error = client
            .stream(request(), None, &Auth::Key("key".into()))
            .await
            .err()
            .expect("refused");

        assert!(error.downcast_ref::<ContextOverflow>().is_none(), "{error}");
        assert!(
            matches!(
                error.downcast_ref::<Error>(),
                Some(Error::Status { status, message, .. })
                    if *status == reqwest::StatusCode::BAD_REQUEST && message == "invalid model"
            ),
            "{error}"
        );
        assert_eq!(retry(&error), None);
    }

    #[test]
    fn only_an_overflow_body_on_an_overflow_status_is_marked() {
        let bad_request = reqwest::StatusCode::BAD_REQUEST;
        let marked = |status, body: &str| {
            status_error(status, body.to_string(), None)
                .downcast_ref::<ContextOverflow>()
                .is_some()
        };

        assert!(marked(
            bad_request,
            "prompt is too long: 213866 tokens > 200000 maximum"
        ));
        assert!(marked(bad_request, r#"{"code":"context_length_exceeded"}"#));
        assert!(marked(
            reqwest::StatusCode::PAYLOAD_TOO_LARGE,
            "input is too long"
        ));
        assert!(marked(
            reqwest::StatusCode::UNPROCESSABLE_ENTITY,
            "PROMPT IS TOO LONG"
        ));

        assert!(!marked(bad_request, "invalid model"));
        assert!(!marked(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            "maximum context length is 8192 tokens"
        ));
        assert!(!marked(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "prompt is too long"
        ));

        let ordinary = status_error(
            bad_request,
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"invalid model"}}"#
                .into(),
            None,
        );
        let status = ordinary.downcast_ref::<Error>().expect("the status error");
        assert!(
            matches!(status, Error::Status { status, message, .. }
                if *status == bad_request && message == "invalid model"),
            "the body still parses to its message"
        );
        assert_eq!(retry(&ordinary), None);

        let limited = status_error(reqwest::StatusCode::TOO_MANY_REQUESTS, String::new(), None);
        assert_eq!(retry(&limited), Some(Retry { after: None }));
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

        let overflow: BoxError = Box::new(ContextOverflow);
        assert_eq!(
            retry(&overflow),
            None,
            "compacting, not retrying, is the fix"
        );

        assert_eq!(
            retry(&boxed(Error::Incomplete)),
            Some(Retry { after: None })
        );
        assert_eq!(retry(&boxed(Error::Truncated)), None);
        let other: BoxError = Box::new(crate::chat_completions::Error::Incomplete);
        assert_eq!(retry(&other), None, "another protocol's error");
    }
}
