//! Anthropic messages over SSE, for the models the catalogue says need it
//! (Claude on OpenCode Zen, MiniMax on Go).

mod event;
mod wire;

use std::collections::BTreeMap;

use futures::stream::BoxStream;
use nth_protocol::{BoxError, Request, StreamEvent, ToolCall, Usage};

use crate::{
    auth::Auth,
    catalog,
    stream::{self, Flow, Out, Payload, Protocol},
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

pub(crate) type Error = stream::Error<Provider>;

/// An `error` event, sent after the 200 status.
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

    /// A rate limit, an overload or a server failure; the kinds stand
    /// for 429, 529 and 500.
    fn retryable(&self) -> bool {
        matches!(
            self.kind.as_str(),
            "rate_limit_error" | "overloaded_error" | "api_error"
        )
    }
}

pub(crate) type Client = stream::Client<State>;

impl Client {
    /// `known` is the catalogue's entry for the model, if it has one: its
    /// output limit bounds `max_tokens`, which is required here, unlike in
    /// chat completions, and its reasoning options say how it thinks.
    pub(crate) async fn stream(
        &self,
        request: Request<'_>,
        known: Option<&catalog::Model>,
        auth: &Auth,
    ) -> Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError> {
        let builder = auth
            .api_key(self.post(), Some(request.session_id))
            .header("anthropic-version", VERSION)
            .json(&wire::body(
                request.model,
                request.effort,
                thinking(known),
                max_tokens(known),
                request.messages,
                request.tools,
            ));
        stream::send::<State>(builder, &request).await
    }
}

/// Text and reasoning go out as they arrive. Tool calls are assembled from
/// their fragments and go out once the stream ends, when they are complete.
#[derive(Default)]
pub(crate) struct State {
    /// Tool calls by the index of their content block.
    calls: BTreeMap<usize, ToolCall>,
    usage: event::Usage,
    stop_reason: Option<String>,
}

impl Protocol for State {
    type Provider = Provider;
    type Event = event::Event;
    const ROUTE: &'static str = "/messages";

    fn parse(line: &str) -> Result<Vec<event::Event>, Error> {
        Ok(event::parse_line(line)?.into_iter().collect())
    }

    fn apply(&mut self, event: event::Event, out: &mut Out<Provider>) -> Flow {
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
                    out.push_back(Ok(StreamEvent::TextDelta(text)));
                }
                event::Delta::Thinking { thinking } if !thinking.is_empty() => {
                    out.push_back(Ok(StreamEvent::ReasoningDelta(thinking)));
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
                out.push_back(Ok(StreamEvent::Usage(Usage {
                    input: self.usage.input(),
                    output: self.usage.output(),
                    cache_read: self.usage.cache_read_input_tokens,
                    cache_write: self.usage.cache_creation_input_tokens,
                    // Anthropic folds thinking into the output count.
                    reasoning: None,
                })));
            }
            event::Event::MessageStop => {
                self.finish(out);
                return Flow::Done;
            }
            event::Event::ContentBlockStart { .. }
            | event::Event::Error { .. }
            | event::Event::Other => {}
        }
        Flow::Continue
    }

    /// The reply is complete once its stop reason arrived; what is lost
    /// with the connection after that is at most the `message_stop` event.
    fn complete(&self) -> bool {
        self.stop_reason.is_some()
    }

    fn finish(&mut self, out: &mut Out<Provider>) {
        // Tool calls cut off at the token limit have partial JSON input and
        // must not run.
        if self.stop_reason.as_deref() == Some("max_tokens") {
            out.push_back(Err(Error::Truncated));
            return;
        }
        let calls = std::mem::take(&mut self.calls);
        out.extend(calls.into_values().map(|mut call| {
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
    use futures::StreamExt;
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
                    reasoning: None,
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
    async fn a_stall_after_the_stop_reason_keeps_the_reply() {
        let head = fixture_until("event: message_stop");
        let bytes =
            futures::stream::iter(chunks(head, head.len())).chain(futures::stream::pending());
        let events: Vec<_> = events(bytes).collect().await;
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
        assert!(
            matches!(error, Error::Provider(Provider { kind, .. }) if kind == "overloaded_error")
        );
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

    /// Anthropic's usual wording: the input plus the requested `max_tokens`
    /// exceed the window — exactly what nth hits on a large model.
    const CONTEXT_LIMIT: &[u8] = br#"{"type":"error","error":{"type":"invalid_request_error","message":"input length and `max_tokens` exceed context limit: 198157 + 32000 > 200000, decrease input length or `max_tokens` and try again"}}"#;
    /// A billing refusal that names a token limit without being about the
    /// input; compacting for it would summarize a session that fits.
    const BILLING: &[u8] = br#"{"type":"error","error":{"type":"invalid_request_error","message":"You have reached your token limit for this month. Update your billing details to continue.","code":"billing_hard_limit_reached"}}"#;

    #[tokio::test]
    async fn a_context_limit_refusal_is_an_overflow_and_a_billing_error_is_not() {
        let (url, _) = serve_status(400, CONTEXT_LIMIT).await;
        let client = Client::new(reqwest::Client::new(), url);
        let error = client
            .stream(request(), None, &Auth::Key("key".into()))
            .await
            .err()
            .expect("refused");
        assert!(error.downcast_ref::<ContextOverflow>().is_some(), "{error}");
        assert_eq!(retry(&error), None, "compacting, not retrying, is the fix");

        let (url, _) = serve_status(400, BILLING).await;
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
                Some(Error::Status { status, .. }) if *status == reqwest::StatusCode::BAD_REQUEST
            ),
            "{error}"
        );
    }

    /// An ordinary refusal that says nothing about a limit.
    const ORDINARY: &[u8] =
        br#"{"error":{"type":"invalid_request_error","message":"invalid model"}}"#;

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

    /// The payload's part of the policy; the rest is `stream`'s.
    #[test]
    fn only_transient_errors_are_retryable() {
        let in_stream = |kind: &str| -> BoxError {
            Box::new(Error::Provider(Provider {
                kind: kind.into(),
                message: String::new(),
            }))
        };
        for kind in ["overloaded_error", "rate_limit_error", "api_error"] {
            assert_eq!(
                retry(&in_stream(kind)),
                Some(Retry { after: None }),
                "{kind}"
            );
        }
        assert_eq!(retry(&in_stream("invalid_request_error")), None);

        let other: BoxError = Box::new(crate::chat_completions::Error::Incomplete);
        assert_eq!(retry(&other), None, "another protocol's error");
    }
}
