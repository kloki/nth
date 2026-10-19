//! OpenAI responses over SSE, for the models the catalogue says need it
//! (GPT on OpenCode Zen, GPT and Grok on Go).

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
    http::{self, STREAM_IDLE_TIMEOUT, retry_after, retryable, transient},
};

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
    /// An `error` event or a failed response, sent after the 200 status.
    /// Also a response stopped for a reason other than its length.
    #[error("provider error {kind}: {message}")]
    Provider { kind: String, message: String },
    #[error("stream ended before the response was complete")]
    Incomplete,
    #[error("stream stalled: nothing arrived for {}s", STREAM_IDLE_TIMEOUT.as_secs())]
    Stalled,
    #[error("response hit the output token limit")]
    Truncated,
}

/// One endpoint's responses route.
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

    /// `reasons` is whether the model is known to reason, which asks for
    /// its summary even when the effort is left to it.
    pub(crate) async fn stream(
        &self,
        request: Request<'_>,
        reasons: bool,
        auth: &Auth,
    ) -> Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError> {
        let signed = auth.bearer(
            self.http.post(format!("{}/responses", self.base_url)),
            Some(request.session_id),
        );
        let response = http::session(signed, &request)
            .json(&wire::body(
                request.model,
                request.session_id,
                request.effort,
                reasons,
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

/// Whether `error`, one a responses `Client` produced, may be retried: the
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
        // A rate limit or a server failure reported inside the stream.
        Error::Provider { kind, .. }
            if matches!(kind.as_str(), "rate_limit_exceeded" | "server_error") =>
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
    /// Tool calls by the index of their output item.
    calls: BTreeMap<usize, ToolCall>,
    finished: bool,
}

/// Text and reasoning go out as they arrive. Tool calls go out once the
/// response is complete, so a response that ends badly runs none.
fn events<S>(bytes: S) -> impl Stream<Item = Result<StreamEvent, Error>>
where
    S: Stream<Item = Result<bytes::Bytes, Error>> + Unpin,
{
    let state = State {
        bytes,
        parser: event::parser(),
        pending: VecDeque::new(),
        calls: BTreeMap::new(),
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
            // Unlike the other two protocols, nothing after the end is
            // tolerated: `response.completed` is the last event and ends
            // the stream at once, so there is no window for the connection
            // to fail in after the reply is complete.
            let (parsed, ended) = match next {
                Ok(Some(Ok(chunk))) => (s.parser.push(&chunk), false),
                Ok(Some(Err(e))) => (Err(e), false),
                Err(_elapsed) => (Err(Error::Stalled), false),
                // A remainder that does not parse is a line the connection
                // cut short, which `Incomplete` describes better than a
                // parse error would.
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
                s.apply(event);
                if s.finished {
                    break;
                }
            }
            // The response ends with its own event, the last the server
            // sends; without it, the connection was cut mid-reply.
            if ended && !s.finished {
                s.finished = true;
                s.pending.push_back(Err(Error::Incomplete));
            }
        }
    })
}

impl<S> State<S> {
    fn apply(&mut self, event: event::Event) {
        match event {
            event::Event::TextDelta { delta } if !delta.is_empty() => {
                self.pending.push_back(Ok(StreamEvent::TextDelta(delta)));
            }
            event::Event::ReasoningDelta { delta } if !delta.is_empty() => {
                self.pending
                    .push_back(Ok(StreamEvent::ReasoningDelta(delta)));
            }
            event::Event::SummaryPart { summary_index } if summary_index > 0 => {
                self.pending
                    .push_back(Ok(StreamEvent::ReasoningDelta("\n\n".into())));
            }
            event::Event::ItemDone {
                output_index,
                item:
                    event::Item::FunctionCall {
                        call_id,
                        name,
                        mut arguments,
                    },
            } => {
                // A call without parameters may come with no arguments at all.
                if arguments.is_empty() {
                    arguments = "{}".into();
                }
                self.calls.insert(
                    output_index,
                    ToolCall {
                        id: call_id,
                        name,
                        arguments,
                    },
                );
            }
            event::Event::Completed { response } => {
                self.usage(&response);
                self.finished = true;
                let calls = std::mem::take(&mut self.calls);
                self.pending
                    .extend(calls.into_values().map(|c| Ok(StreamEvent::ToolCall(c))));
            }
            // Tool calls of a response stopped early may be partial, so
            // none run.
            event::Event::Incomplete { response } => {
                self.usage(&response);
                self.finished = true;
                let reason = response.incomplete_details.and_then(|d| d.reason);
                self.pending.push_back(Err(match reason.as_deref() {
                    Some("max_output_tokens") => Error::Truncated,
                    _ => Error::Provider {
                        kind: reason.unwrap_or_default(),
                        message: "response stopped early".into(),
                    },
                }));
            }
            _ => {}
        }
    }

    fn usage(&mut self, response: &event::Response) {
        if let Some(usage) = response.usage {
            self.pending.push_back(Ok(StreamEvent::Usage(Usage {
                input: usage.input_tokens,
                output: usage.output_tokens,
                cache_read: usage.input_tokens_details.and_then(|d| d.cached_tokens),
                cache_write: usage
                    .input_tokens_details
                    .and_then(|d| d.cache_write_tokens),
                reasoning: usage.output_tokens_details.and_then(|d| d.reasoning_tokens),
            })));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{chunks, runs_no_tools, serve_status, until};

    /// Recorded from OpenCode Zen's gpt-5.4-nano at low effort, shortened:
    /// one delta per summary part, encrypted reasoning and the repeated
    /// response objects cut down.
    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/zen_responses.sse");

    /// OpenAI's refusal on its responses API.
    const OVERFLOW: &[u8] = br#"{"error":{"message":"Your input exceeds the context window of this model. Please adjust your input and try again.","type":"invalid_request_error","param":null,"code":"context_length_exceeded"}}"#;

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
                StreamEvent::ReasoningDelta("**Choosing glob patterns**".into()),
                StreamEvent::ReasoningDelta("\n\n".into()),
                StreamEvent::ReasoningDelta("Both at once.".into()),
                StreamEvent::TextDelta("I’ll read wörld".into()),
                StreamEvent::TextDelta(" files.".into()),
                StreamEvent::Usage(Usage {
                    input: 1251,
                    output: 225,
                    cache_read: Some(1152),
                    cache_write: Some(0),
                    reasoning: Some(142),
                }),
                StreamEvent::ToolCall(ToolCall {
                    id: "call_y8Tp".into(),
                    name: "read".into(),
                    arguments: r#"{"filePath":"Cargo.toml"}"#.into(),
                }),
                StreamEvent::ToolCall(ToolCall {
                    id: "call_xzh9".into(),
                    name: "glob".into(),
                    arguments: r#"{"pattern":"**/*.rs"}"#.into(),
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
    async fn the_completed_response_ends_the_stream() {
        let mut input = chunks(FIXTURE, FIXTURE.len());
        input.push(Err(Error::Incomplete));
        let events: Vec<_> = events(futures::stream::iter(input)).collect().await;
        let events: Vec<_> = events.into_iter().map(|e| e.expect("valid")).collect();
        assert_eq!(events, replay(FIXTURE.len()).await);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_stream_is_a_retryable_error() {
        let head = fixture_until("event: response.completed");
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
        let events = replay_bytes(fixture_until("event: response.completed"), FIXTURE.len()).await;
        assert!(matches!(events.last(), Some(Err(Error::Incomplete))));
        assert!(runs_no_tools(&events));
    }

    #[tokio::test]
    async fn output_limit_is_an_error_and_runs_no_tools() {
        let text = std::str::from_utf8(FIXTURE).expect("utf-8 fixture");
        let input = text
            .replace("response.completed", "response.incomplete")
            .replace(
                r#""incomplete_details":null,"output":[]"#,
                r#""incomplete_details":{"reason":"max_output_tokens"},"output":[]"#,
            );
        let events = replay_bytes(input.as_bytes(), input.len()).await;
        assert!(matches!(events.last(), Some(Err(Error::Truncated))));
        assert!(
            events.iter().any(|e| matches!(
                e,
                Ok(StreamEvent::Usage(Usage {
                    input: 1251,
                    output: 225,
                    ..
                }))
            )),
            "{events:?}"
        );
        assert!(runs_no_tools(&events));

        let filtered = input.replace("max_output_tokens", "content_filter");
        let events = replay_bytes(filtered.as_bytes(), filtered.len()).await;
        assert!(matches!(
            events.last(),
            Some(Err(Error::Provider { kind, .. })) if kind == "content_filter"
        ));
    }

    #[tokio::test]
    async fn an_error_event_ends_the_stream() {
        let input = [
            fixture_until("event: response.output_item.added"),
            b"event: error\ndata: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"Upstream failed\",\"param\":null}\n\n",
        ]
        .concat();
        let events = replay_bytes(&input, input.len()).await;
        let Some(Err(error)) = events.last() else {
            panic!("expected an error, got {events:?}");
        };
        assert!(matches!(error, Error::Provider { kind, .. } if kind == "server_error"));
    }

    #[tokio::test]
    async fn an_overflow_refusal_becomes_a_context_overflow() {
        let (url, _) = serve_status(400, OVERFLOW).await;
        let client = Client::new(reqwest::Client::new(), url);
        let error = client
            .stream(request(), false, &Auth::Key("key".into()))
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
    const ORDINARY: &[u8] = br#"{"error":{"message":"invalid model"}}"#;

    #[tokio::test]
    async fn any_overflow_phrasing_on_a_400_413_or_422_becomes_a_context_overflow() {
        for status in [400, 413, 422] {
            for body in [NESTED_OVERFLOW, PLAIN_OVERFLOW] {
                let (url, _) = serve_status(status, body).await;
                let client = Client::new(reqwest::Client::new(), url);
                let error = client
                    .stream(request(), false, &Auth::Key("key".into()))
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
                .stream(request(), false, &Auth::Key("key".into()))
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
            .stream(request(), false, &Auth::Key("key".into()))
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
            "Your input exceeds the context window of this model (context_length_exceeded)"
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

        assert!(!marked(
            bad_request,
            r#"{"error":{"code":"invalid_prompt","message":"invalid model"}}"#
        ));
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
            r#"{"error":{"code":"invalid_prompt","message":"invalid model"}}"#.into(),
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
        assert_eq!(
            retry(&status(reqwest::StatusCode::BAD_GATEWAY, None)),
            Some(Retry { after: None })
        );
        assert_eq!(retry(&status(reqwest::StatusCode::BAD_REQUEST, None)), None);
        assert_eq!(
            retry(&status(reqwest::StatusCode::UNAUTHORIZED, None)),
            None
        );

        for kind in ["server_error", "rate_limit_exceeded"] {
            assert_eq!(
                retry(&in_stream(kind)),
                Some(Retry { after: None }),
                "{kind}"
            );
        }
        assert_eq!(retry(&in_stream("invalid_prompt")), None);
        assert_eq!(retry(&in_stream("content_filter")), None);

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
        let other: BoxError = Box::new(crate::messages::Error::Incomplete);
        assert_eq!(retry(&other), None, "another protocol's error");
    }
}
