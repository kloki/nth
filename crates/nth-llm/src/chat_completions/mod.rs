//! OpenAI-compatible chat completions over SSE, for OpenCode Go and any
//! other endpoint speaking this protocol, and what a model goes over when
//! the catalogue says nothing else.

mod event;
mod wire;

use std::collections::BTreeMap;

use futures::{StreamExt, stream::BoxStream};
use nth_protocol::{BoxError, Request, StreamEvent, ToolCall, Usage};

use crate::{
    auth::Auth,
    http::{self, transient},
    stream::{self, Flow, Out, Payload, Protocol},
};

pub(crate) type Error = stream::Error<Provider>;

/// An error chunk, which gateways such as OpenRouter send after the 200
/// status for a rate limit, an overload or a refusal.
#[derive(Debug, thiserror::Error)]
#[error("provider error{}: {message}", .status.map(|s| format!(" {s}")).unwrap_or_default())]
pub(crate) struct Provider {
    pub(crate) message: String,
    /// The HTTP status the error stands for, when the chunk gave one.
    pub(crate) status: Option<reqwest::StatusCode>,
}

impl Payload for Provider {
    /// Only by its status: a symbolic code such as `rate_limit_exceeded`
    /// says nothing every gateway agrees on.
    fn retryable(&self) -> bool {
        self.status.is_some_and(transient)
    }

    fn context_overflow(&self) -> bool {
        self.status
            .is_some_and(|status| http::context_overflow(status, &self.message))
    }
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
        let response = stream::success::<Provider>(response).await?;

        let bytes = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(Error::from))
            .boxed();
        Ok(stream::events::<State, _>(bytes)
            .map(|e| e.map_err(stream::boxed))
            .boxed())
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

/// Text and reasoning go out as they arrive. Tool calls are assembled from
/// their fragments and go out once the stream ends, when they are complete.
#[derive(Default)]
pub(crate) struct State {
    calls: BTreeMap<usize, ToolCall>,
    finish_reason: Option<String>,
}

impl Protocol for State {
    type Provider = Provider;
    type Event = event::Event;

    fn parse(line: &str) -> Result<Vec<event::Event>, Error> {
        event::parse_line(line)
    }

    fn apply(&mut self, event: event::Event, out: &mut Out<Provider>) -> Flow {
        match event {
            event::Event::Delta(delta) => self.delta(delta, out),
            event::Event::Finish(reason) => self.finish_reason = Some(reason),
            event::Event::Usage(usage) => out.push_back(Ok(StreamEvent::Usage(Usage {
                input: usage.prompt_tokens,
                output: usage.completion_tokens,
                cache_read: usage.cache_read(),
                // No compatible server reports a cache write.
                cache_write: None,
                reasoning: usage.reasoning(),
            }))),
            event::Event::Done => {
                self.finish(out);
                return Flow::Done;
            }
        }
        Flow::Continue
    }

    /// The reply is complete once the finish chunk arrived; what is lost
    /// with the connection after that is at most the `[DONE]` line.
    fn complete(&self) -> bool {
        self.finish_reason.is_some()
    }

    fn finish(&mut self, out: &mut Out<Provider>) {
        // Tool calls cut off at the token limit have partial JSON
        // arguments and must not run.
        if self.finish_reason.as_deref() == Some("length") {
            out.push_back(Err(Error::Truncated));
            return;
        }
        let calls = std::mem::take(&mut self.calls);
        out.extend(calls.into_values().map(|c| Ok(StreamEvent::ToolCall(c))));
    }
}

impl State {
    fn delta(&mut self, delta: event::Delta, out: &mut Out<Provider>) {
        if let Some(text) = delta.reasoning_content.filter(|t| !t.is_empty()) {
            out.push_back(Ok(StreamEvent::ReasoningDelta(text)));
        }
        if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
            out.push_back(Ok(StreamEvent::TextDelta(text)));
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
    use std::time::Duration;

    use nth_protocol::{ContextOverflow, Retry};

    use super::*;
    use crate::testing::{chunks, runs_no_tools, serve_once, serve_status, until};

    fn retry(error: &BoxError) -> Option<Retry> {
        stream::retry::<Provider>(error)
    }

    fn events<S>(bytes: S) -> impl futures::Stream<Item = Result<StreamEvent, Error>>
    where
        S: futures::Stream<Item = Result<bytes::Bytes, Error>> + Unpin,
    {
        stream::events::<State, S>(bytes)
    }

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

    /// As a body error after the finish chunk: what goes quiet then is at
    /// most the `[DONE]` line.
    #[tokio::test(start_paused = true)]
    async fn a_stall_after_finish_keeps_the_reply() {
        let head = fixture_until("data: [DONE]");
        let bytes =
            futures::stream::iter(chunks(head, head.len())).chain(futures::stream::pending());
        let events: Vec<_> = events(bytes).collect().await;
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
                Some(Error::Status { status, message, .. })
                    if *status == reqwest::StatusCode::BAD_REQUEST
                        && message.as_bytes() == ORDINARY
            ),
            "{error}"
        );
        assert_eq!(retry(&error), None);
    }

    #[test]
    fn only_an_overflow_body_on_an_overflow_status_is_marked() {
        let bad_request = reqwest::StatusCode::BAD_REQUEST;
        let marked = |status, body: &str| {
            stream::status_error::<Provider>(status, body.to_string(), None)
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

        let ordinary = stream::status_error::<Provider>(
            bad_request,
            r#"{"error":{"message":"invalid model"}}"#.into(),
            None,
        );
        let status = ordinary.downcast_ref::<Error>().expect("the status error");
        assert!(matches!(
            status,
            Error::Status { status, message, .. }
                if *status == bad_request && message == r#"{"error":{"message":"invalid model"}}"#
        ));
        assert_eq!(retry(&ordinary), None);

        let limited = stream::status_error::<Provider>(
            reqwest::StatusCode::TOO_MANY_REQUESTS,
            String::new(),
            None,
        );
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
            message: String::new(),
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
            message: String::new(),
            retry_after: None,
        });
        assert_eq!(retry(&overloaded), Some(Retry { after: None }));

        let rejected = boxed(Error::Status {
            status: reqwest::StatusCode::BAD_REQUEST,
            message: String::new(),
            retry_after: None,
        });
        assert_eq!(retry(&rejected), None);
        assert_eq!(
            retry(&boxed(Error::Incomplete)),
            Some(Retry { after: None })
        );
        assert_eq!(retry(&boxed(Error::Truncated)), None);

        let in_stream = |status: Option<reqwest::StatusCode>| {
            boxed(Error::Provider(Provider {
                message: "from the gateway".into(),
                status,
            }))
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
