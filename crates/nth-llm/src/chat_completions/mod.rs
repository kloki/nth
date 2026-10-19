//! OpenAI-compatible chat completions over SSE, for OpenCode Go and any
//! other endpoint speaking this protocol, and what a model goes over when
//! the catalogue says nothing else.

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
    #[error("{status}: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
        /// The server's `Retry-After`, when it sent one.
        retry_after: Option<Duration>,
    },
    #[error("bad stream chunk: {source} in {line:?}")]
    Parse {
        source: serde_json::Error,
        /// The start of the offending line; see `crate::event::EXCERPT_CHARS`.
        line: String,
    },
    #[error("provider error{}: {message}", .status.map(|s| format!(" {s}")).unwrap_or_default())]
    Provider {
        message: String,
        /// The HTTP status the error stands for, when the chunk gave one.
        status: Option<reqwest::StatusCode>,
    },
    #[error("stream ended before the response was complete")]
    Incomplete,
    #[error("stream stalled: nothing arrived for {}s", STREAM_IDLE_TIMEOUT.as_secs())]
    Stalled,
    #[error("response hit the output token limit")]
    Truncated,
}

/// One endpoint's chat completions route.
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

    /// `cache_key` goes out as `prompt_cache_key`: only to a provider known
    /// to take it ([`takes_cache_key`]), since strict ones refuse a field
    /// they do not know.
    pub(crate) async fn stream(
        &self,
        request: Request<'_>,
        cache_key: Option<&str>,
        auth: &Auth,
    ) -> Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError> {
        let signed = auth.bearer(
            self.http
                .post(format!("{}/chat/completions", self.base_url)),
            Some(request.session_id),
        );
        let response = http::session(signed, &request)
            .json(&wire::body(
                request.model,
                request.effort,
                cache_key,
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
        Ok(events(bytes).map(|e| e.map_err(streamed_error)).boxed())
    }
}

/// The models.dev ids of the providers whose chat completions take
/// `prompt_cache_key`, as opencode lists them.
const CACHE_KEY_PROVIDERS: &[&str] = &[
    "abliteration-ai",
    "aihubmix",
    "anyapi",
    "auriko",
    "azure",
    "cerebras",
    "deepinfra",
    "digitalocean",
    "edenai",
    "fireworks-ai",
    "jalapeno",
    "llama",
    "meta",
    "moonshotai",
    "moonshotai-cn",
    "morph",
    "openai",
    "openrouter",
    "snowflake-cortex",
    "tencent-tokenhub",
    "upstage",
    "venice",
];

/// Whether the provider of models.dev id `provider` takes a
/// `prompt_cache_key`.
pub(crate) fn takes_cache_key(provider: &str) -> bool {
    CACHE_KEY_PROVIDERS.contains(&provider)
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
    Box::new(Error::Status {
        status,
        body,
        retry_after,
    })
}

/// A stream's error: an in-band error that names an overflow status and
/// body becomes the marker too, so a gateway that reports one after the
/// 200 status is compacted like one that refused upfront. Everything else
/// stays the protocol's error.
fn streamed_error(error: Error) -> BoxError {
    if let Error::Provider {
        message,
        status: Some(status),
    } = &error
        && http::context_overflow(*status, message)
    {
        return Box::new(ContextOverflow);
    }
    Box::new(error)
}

/// Whether `error`, one a chat completions `Client` produced, may be
/// retried. Reads only the error, so whatever fronts the clients answers
/// the same way.
pub(crate) fn retry(error: &BoxError) -> Option<Retry> {
    let error = error.downcast_ref::<Error>()?;
    match error {
        // A rate limit or a transient server failure; the server may
        // have said how long to wait.
        Error::Status {
            status,
            retry_after,
            ..
        } if transient(*status) => Some(Retry {
            after: *retry_after,
        }),
        // The same, reported inside the stream after a 200 status.
        Error::Provider {
            status: Some(status),
            ..
        } if transient(*status) => Some(Retry { after: None }),
        Error::Http(e) if retryable(e) => Some(Retry { after: None }),
        // The stream ended, or went quiet, before the reply did.
        Error::Incomplete | Error::Stalled => Some(Retry { after: None }),
        _ => None,
    }
}

struct State<S> {
    bytes: S,
    parser: event::Parser,
    pending: VecDeque<Result<StreamEvent, Error>>,
    calls: BTreeMap<usize, ToolCall>,
    finish_reason: Option<String>,
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
        finish_reason: None,
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
                // The reply is complete once the finish chunk arrived; what
                // is lost with the connection after that is at most the
                // `[DONE]` line, not worth failing the turn over.
                Ok(Some(Err(_))) | Err(_) if s.finish_reason.is_some() => (Ok(Vec::new()), true),
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
                match event {
                    event::Event::Delta(delta) => s.apply(delta),
                    event::Event::Finish(reason) => s.finish_reason = Some(reason),
                    event::Event::Usage(usage) => {
                        s.pending.push_back(Ok(StreamEvent::Usage(Usage {
                            input: usage.prompt_tokens,
                            output: usage.completion_tokens,
                            cache_read: usage.cache_read(),
                            // No compatible server reports a cache write.
                            cache_write: None,
                            reasoning: usage.reasoning(),
                        })))
                    }
                    event::Event::Done => {
                        s.done();
                        break;
                    }
                }
            }
            if ended && !s.finished {
                // Some servers close right after the finish chunk without
                // `[DONE]`; without either, the connection was cut mid-reply.
                if s.finish_reason.is_some() {
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
    fn done(&mut self) {
        self.finished = true;
        // Tool calls cut off at the token limit have partial JSON
        // arguments and must not run.
        if self.finish_reason.as_deref() == Some("length") {
            self.pending.push_back(Err(Error::Truncated));
            return;
        }
        let calls = std::mem::take(&mut self.calls);
        self.pending
            .extend(calls.into_values().map(|c| Ok(StreamEvent::ToolCall(c))));
    }

    fn apply(&mut self, delta: event::Delta) {
        if let Some(text) = delta.reasoning_content.filter(|t| !t.is_empty()) {
            self.pending
                .push_back(Ok(StreamEvent::ReasoningDelta(text)));
        }
        if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
            self.pending.push_back(Ok(StreamEvent::TextDelta(text)));
        }
        for part in delta.tool_calls.into_iter().flatten() {
            let call = self.calls.entry(part.index).or_insert_with(|| ToolCall {
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
            });
            if let Some(id) = part.id {
                call.id = id;
            }
            if let Some(function) = part.function {
                if let Some(name) = function.name {
                    call.name.push_str(&name);
                }
                if let Some(arguments) = function.arguments {
                    call.arguments.push_str(&arguments);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{chunks, runs_no_tools, serve_once, serve_status, until};

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/go_stream.sse");

    /// OpenAI's refusal, as its API sends it.
    const OVERFLOW: &[u8] = br#"{"error":{"message":"This model's maximum context length is 8192 tokens. However, you requested 8896 tokens (8896 in the messages, 0 in the completion). Please reduce the length of the messages.","type":"invalid_request_error","param":null,"code":"context_length_exceeded"}}"#;

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
                StreamEvent::ReasoningDelta("Let me look.".into()),
                StreamEvent::TextDelta("Reading wörld".into()),
                StreamEvent::TextDelta(" files.".into()),
                StreamEvent::Usage(Usage {
                    input: 5,
                    output: 3,
                    cache_read: None,
                    cache_write: None,
                    reasoning: None,
                }),
                StreamEvent::ToolCall(ToolCall {
                    id: "call_a".into(),
                    name: "read".into(),
                    arguments: r#"{"filePath":"Cargo.toml"}"#.into(),
                }),
                StreamEvent::ToolCall(ToolCall {
                    id: "call_b".into(),
                    name: "bash".into(),
                    arguments: r#"{"command":"ls"}"#.into(),
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
    async fn mistral_thinking_chunks_are_reasoning() {
        let fixture = include_bytes!("../../tests/fixtures/zen_mistral.sse");
        let whole: Vec<_> = replay_bytes(fixture, fixture.len())
            .await
            .into_iter()
            .map(|e| e.expect("fixture is valid"))
            .collect();
        for size in [1, 7, 64] {
            let events: Vec<_> = replay_bytes(fixture, size)
                .await
                .into_iter()
                .map(|e| e.expect("fixture is valid"))
                .collect();
            assert_eq!(events, whole, "chunk size {size}");
        }

        let (mut reasoning, mut text, mut rest) = (String::new(), String::new(), Vec::new());
        for event in whole {
            match event {
                StreamEvent::ReasoningDelta(delta) => reasoning.push_str(&delta),
                StreamEvent::TextDelta(delta) => text.push_str(&delta),
                other => rest.push(other),
            }
        }
        assert!(reasoning.starts_with("The user wants me to"), "{reasoning}");
        assert_eq!(text, "Reading the project manifest now.");
        assert_eq!(
            rest,
            vec![
                StreamEvent::Usage(Usage {
                    input: 74,
                    output: 58,
                    cache_read: Some(0),
                    cache_write: None,
                    reasoning: None,
                }),
                StreamEvent::ToolCall(ToolCall {
                    id: "WeYavUuRu".into(),
                    name: "read".into(),
                    arguments: r#"{"path": "Cargo.toml"}"#.into(),
                }),
            ]
        );
    }

    #[tokio::test]
    async fn missing_done_after_finish_reason_is_fine() {
        let events = replay_bytes(fixture_until("data: [DONE]"), FIXTURE.len()).await;
        let events: Vec<_> = events.into_iter().map(|e| e.expect("valid")).collect();
        assert_eq!(events, replay(FIXTURE.len()).await);
    }

    #[tokio::test]
    async fn last_line_without_a_newline_is_parsed() {
        let text = std::str::from_utf8(fixture_until("data: [DONE]")).expect("utf-8 fixture");
        let input = text.trim_end();
        assert!(input.ends_with('}'), "the usage chunk has no newline");
        let events = replay_bytes(input.as_bytes(), input.len()).await;
        let events: Vec<_> = events.into_iter().map(|e| e.expect("valid")).collect();
        assert_eq!(events, replay(FIXTURE.len()).await);
    }

    #[tokio::test]
    async fn body_error_after_finish_keeps_the_reply() {
        let mut input = chunks(fixture_until("data: [DONE]"), FIXTURE.len());
        input.push(Err(Error::Incomplete));
        let events: Vec<_> = events(futures::stream::iter(input)).collect().await;
        let events: Vec<_> = events.into_iter().map(|e| e.expect("valid")).collect();
        assert_eq!(events, replay(FIXTURE.len()).await);
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_stream_is_a_retryable_error() {
        let head = fixture_until("\"finish_reason\"");
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
        let events = replay_bytes(fixture_until("\"finish_reason\""), FIXTURE.len()).await;
        assert!(matches!(events.last(), Some(Err(Error::Incomplete))));
        assert!(runs_no_tools(&events));
    }

    #[tokio::test]
    async fn length_finish_is_an_error_and_runs_no_tools() {
        let text = std::str::from_utf8(FIXTURE).expect("utf-8 fixture");
        let input = text.replace(
            "\"finish_reason\":\"tool_calls\"",
            "\"finish_reason\":\"length\"",
        );
        let events = replay_bytes(input.as_bytes(), input.len()).await;
        assert!(matches!(events.last(), Some(Err(Error::Truncated))));
        assert!(runs_no_tools(&events));
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

    #[tokio::test]
    async fn an_in_stream_overflow_becomes_a_context_overflow() {
        const BODY: &[u8] = b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
            data: {\"error\":{\"message\":\"This model's maximum context length is 8192 tokens\",\"code\":400}}\n\n";
        let (url, _) = serve_once(BODY).await;
        let client = Client::new(reqwest::Client::new(), url);
        let events: Vec<_> = client
            .stream(request(), None, &Auth::Key("key".into()))
            .await
            .expect("streams")
            .collect()
            .await;
        let Some(Err(error)) = events.last() else {
            panic!("expected an error, got {events:?}");
        };
        assert!(error.downcast_ref::<ContextOverflow>().is_some(), "{error}");
        assert_eq!(retry(error), None, "compacting, not retrying, is the fix");
    }

    /// The refusal phrased in a nested field, and as plain text: the
    /// classification reads the whole body, not one field of it.
    const NESTED_OVERFLOW: &[u8] = br#"{"error":{"message":"Request failed","detail":{"reason":"maximum context length exceeded"}}}"#;
    const PLAIN_OVERFLOW: &[u8] = b"Input is too long for requested model";
    /// An ordinary refusal that says nothing about a limit.
    const ORDINARY: &[u8] =
        br#"{"error":{"message":"invalid model","type":"invalid_request_error"}}"#;

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
                Some(Error::Status { status, body, .. })
                    if *status == reqwest::StatusCode::BAD_REQUEST && body.contains("invalid model")
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
            "This model's maximum context length is 8192 tokens"
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
            r#"{"error":{"message":"invalid model"}}"#
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
            r#"{"error":{"message":"invalid model"}}"#.into(),
            None,
        );
        let status = ordinary.downcast_ref::<Error>().expect("the status error");
        assert!(matches!(
            status,
            Error::Status { status, body, .. }
                if *status == bad_request && body.contains("invalid model")
        ));
        assert_eq!(retry(&ordinary), None);

        let limited = status_error(reqwest::StatusCode::TOO_MANY_REQUESTS, String::new(), None);
        assert_eq!(retry(&limited), Some(Retry { after: None }));
    }

    #[test]
    fn only_transient_errors_are_retryable() {
        let boxed = |error: Error| -> BoxError { Box::new(error) };

        let overflow: BoxError = Box::new(ContextOverflow);
        assert_eq!(
            retry(&overflow),
            None,
            "compacting, not retrying, is the fix"
        );

        let limited = boxed(Error::Status {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            body: String::new(),
            retry_after: Some(Duration::from_secs(7)),
        });
        assert_eq!(
            retry(&limited),
            Some(Retry {
                after: Some(Duration::from_secs(7))
            })
        );

        let overloaded = boxed(Error::Status {
            status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
            body: String::new(),
            retry_after: None,
        });
        assert_eq!(retry(&overloaded), Some(Retry { after: None }));

        let rejected = boxed(Error::Status {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: String::new(),
            retry_after: None,
        });
        assert_eq!(retry(&rejected), None);
        assert_eq!(
            retry(&boxed(Error::Incomplete)),
            Some(Retry { after: None })
        );
        assert_eq!(retry(&boxed(Error::Truncated)), None);

        let in_stream = |status: Option<reqwest::StatusCode>| {
            boxed(Error::Provider {
                message: "from the gateway".into(),
                status,
            })
        };
        assert_eq!(
            retry(&in_stream(Some(reqwest::StatusCode::TOO_MANY_REQUESTS))),
            Some(Retry { after: None })
        );
        assert_eq!(
            retry(&in_stream(Some(reqwest::StatusCode::BAD_GATEWAY))),
            Some(Retry { after: None })
        );
        assert_eq!(
            retry(&in_stream(Some(reqwest::StatusCode::BAD_REQUEST))),
            None
        );
        assert_eq!(retry(&in_stream(None)), None);

        let bad_url = reqwest::Client::new()
            .get("not a url")
            .build()
            .expect_err("not a url");
        assert_eq!(retry(&boxed(Error::Http(bad_url))), None);
    }
}
