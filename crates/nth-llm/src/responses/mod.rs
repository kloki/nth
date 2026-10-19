//! OpenAI responses over SSE, for the models the catalogue says need it
//! (GPT on OpenCode Zen, GPT and Grok on Go).

mod event;
mod wire;

use std::collections::BTreeMap;

use futures::{StreamExt, stream::BoxStream};
use nth_protocol::{BoxError, Request, StreamEvent, ToolCall, Usage};

use crate::{
    auth::Auth,
    http,
    stream::{self, Flow, Out, Payload, Protocol},
};

pub(crate) type Error = stream::Error<Provider>;

/// An `error` event or a failed response, sent after the 200 status.
/// Also a response stopped for a reason other than its length.
#[derive(Debug, thiserror::Error)]
#[error("provider error {kind}: {message}")]
pub(crate) struct Provider {
    pub(crate) kind: String,
    pub(crate) message: String,
}

impl Payload for Provider {
    fn status_message(body: &str) -> Option<String> {
        serde_json::from_str::<event::ErrorResponse>(body)
            .ok()
            .map(|error| error.error.message)
    }

    /// A rate limit or a server failure.
    fn retryable(&self) -> bool {
        matches!(self.kind.as_str(), "rate_limit_exceeded" | "server_error")
    }
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

/// Text and reasoning go out as they arrive. Tool calls go out once the
/// response is complete, so a response that ends badly runs none.
///
/// Unlike the other two protocols, nothing after the end is tolerated, so
/// it keeps `complete`'s default: `response.completed` is the last event
/// and ends the stream at once, so there is no window for the connection
/// to fail in after the reply is complete.
#[derive(Default)]
pub(crate) struct State {
    /// Tool calls by the index of their output item.
    calls: BTreeMap<usize, ToolCall>,
}

impl Protocol for State {
    type Provider = Provider;
    type Event = event::Event;

    fn parse(line: &str) -> Result<Vec<event::Event>, Error> {
        Ok(event::parse_line(line)?.into_iter().collect())
    }

    fn apply(&mut self, event: event::Event, out: &mut Out<Provider>) -> Flow {
        match event {
            event::Event::TextDelta { delta } if !delta.is_empty() => {
                out.push_back(Ok(StreamEvent::TextDelta(delta)));
            }
            event::Event::ReasoningDelta { delta } if !delta.is_empty() => {
                out.push_back(Ok(StreamEvent::ReasoningDelta(delta)));
            }
            event::Event::SummaryPart { summary_index } if summary_index > 0 => {
                out.push_back(Ok(StreamEvent::ReasoningDelta("\n\n".into())));
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
                usage(&response, out);
                let calls = std::mem::take(&mut self.calls);
                out.extend(calls.into_values().map(|c| Ok(StreamEvent::ToolCall(c))));
                return Flow::Done;
            }
            // Tool calls of a response stopped early may be partial, so
            // none run.
            event::Event::Incomplete { response } => {
                usage(&response, out);
                let reason = response.incomplete_details.and_then(|d| d.reason);
                out.push_back(Err(match reason.as_deref() {
                    Some("max_output_tokens") => Error::Truncated,
                    _ => Error::Provider(Provider {
                        kind: reason.unwrap_or_default(),
                        message: "response stopped early".into(),
                    }),
                }));
                return Flow::Done;
            }
            _ => {}
        }
        Flow::Continue
    }
}

fn usage(response: &event::Response, out: &mut Out<Provider>) {
    if let Some(usage) = response.usage {
        out.push_back(Ok(StreamEvent::Usage(Usage {
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nth_protocol::{ContextOverflow, Retry};

    use super::*;
    use crate::testing::{chunks, runs_no_tools, serve_status, until};

    fn retry(error: &BoxError) -> Option<Retry> {
        stream::retry::<Provider>(error)
    }

    fn events<S>(bytes: S) -> impl futures::Stream<Item = Result<StreamEvent, Error>>
    where
        S: futures::Stream<Item = Result<bytes::Bytes, Error>> + Unpin,
    {
        stream::events::<State, S>(bytes)
    }

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

    /// Unlike the other two protocols, nothing short of the closing event
    /// is a complete reply, so the connection failing before it is the
    /// stream's error, and no tool call runs.
    #[tokio::test]
    async fn a_body_error_before_the_completed_response_is_the_error() {
        let bad_url = reqwest::Client::new()
            .get("not a url")
            .build()
            .expect_err("not a url");
        let mut input = chunks(fixture_until("event: response.completed"), FIXTURE.len());
        input.push(Err(Error::Http(bad_url)));
        let events: Vec<_> = events(futures::stream::iter(input)).collect().await;
        assert!(
            matches!(events.last(), Some(Err(Error::Http(_)))),
            "{events:?}"
        );
        assert!(runs_no_tools(&events));
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
            Some(Err(Error::Provider(Provider { kind, .. }))) if kind == "content_filter"
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
        assert!(matches!(error, Error::Provider(Provider { kind, .. }) if kind == "server_error"));
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
            stream::status_error::<Provider>(status, body.to_string(), None)
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

        let ordinary = stream::status_error::<Provider>(
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
        let status = |status, retry_after| {
            boxed(Error::Status {
                status,
                message: String::new(),
                retry_after,
            })
        };
        let in_stream = |kind: &str| {
            boxed(Error::Provider(Provider {
                kind: kind.into(),
                message: String::new(),
            }))
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
