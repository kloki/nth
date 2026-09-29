//! OpenAI-compatible chat completions over SSE. Works against OpenCode Go and
//! any other endpoint speaking this protocol.

mod models;
mod sse;
mod wire;

use std::collections::{BTreeMap, VecDeque};

use futures::{FutureExt, Stream, StreamExt, future::BoxFuture, stream::BoxStream};
use nth_protocol::{BoxError, ModelInfo, Provider, Request, StreamEvent, ToolCall};

const USER_AGENT: &str = concat!("nth/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{status}: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("bad stream chunk: {source} in {line:?}")]
    Parse {
        source: serde_json::Error,
        line: String,
    },
    #[error("provider error: {0}")]
    Provider(String),
    #[error("stream ended before the response was complete")]
    Incomplete,
    #[error("response hit the output token limit")]
    Truncated,
}

pub struct ChatClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    session_id: String,
}

impl ChatClient {
    /// `session_id` must stay stable for a conversation: Go routes and caches
    /// prompts on the `x-opencode-session` header.
    pub fn new(base_url: String, api_key: String, session_id: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            session_id,
        }
    }

    async fn open(
        &self,
        request: Request<'_>,
    ) -> Result<BoxStream<'static, Result<StreamEvent, BoxError>>, Error> {
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header("x-opencode-session", &self.session_id)
            .json(&wire::body(request.model, request.messages, request.tools))
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

/// Turns a non-2xx response into an error that carries the server's body.
async fn success(response: reqwest::Response) -> Result<reqwest::Response, Error> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    Err(Error::Status { status, body })
}

impl Provider for ChatClient {
    fn models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, BoxError>> {
        async move {
            models::list(&self.http, &self.base_url, &self.api_key)
                .await
                .map_err(BoxError::from)
        }
        .boxed()
    }

    fn stream<'a>(
        &'a self,
        request: Request<'a>,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>> {
        async move { self.open(request).await.map_err(BoxError::from) }.boxed()
    }
}

struct State<S> {
    bytes: S,
    parser: sse::Parser,
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
        parser: sse::Parser::default(),
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
            let parsed = match s.bytes.next().await {
                Some(Ok(chunk)) => s.parser.push(&chunk),
                Some(Err(e)) => Err(e),
                // Some servers close right after the finish chunk without
                // `[DONE]`; without either, the connection was cut mid-reply.
                None if s.finish_reason.is_some() => Ok(vec![sse::Event::Done]),
                None => Err(Error::Incomplete),
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
                    sse::Event::Delta(delta) => s.apply(delta),
                    sse::Event::Finish(reason) => s.finish_reason = Some(reason),
                    sse::Event::Done => {
                        s.finished = true;
                        // Tool calls cut off at the token limit have partial
                        // JSON arguments and must not run.
                        if s.finish_reason.as_deref() == Some("length") {
                            s.pending.push_back(Err(Error::Truncated));
                            break;
                        }
                        let calls = std::mem::take(&mut s.calls);
                        s.pending
                            .extend(calls.into_values().map(|c| Ok(StreamEvent::ToolCall(c))));
                        break;
                    }
                }
            }
        }
    })
}

impl<S> State<S> {
    fn apply(&mut self, delta: sse::Delta) {
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

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/go_stream.sse");

    async fn replay(chunk_size: usize) -> Vec<StreamEvent> {
        replay_bytes(FIXTURE, chunk_size)
            .await
            .into_iter()
            .map(|e| e.expect("fixture is valid"))
            .collect()
    }

    async fn replay_bytes(input: &[u8], chunk_size: usize) -> Vec<Result<StreamEvent, Error>> {
        let chunks = input
            .chunks(chunk_size)
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect::<Vec<_>>();
        events(futures::stream::iter(chunks)).collect().await
    }

    fn fixture_until(marker: &str) -> &'static [u8] {
        let text = std::str::from_utf8(FIXTURE).expect("utf-8 fixture");
        let end = text.find(marker).expect("marker in fixture");
        &FIXTURE[..end]
    }

    #[tokio::test]
    async fn assembles_text_reasoning_and_tool_calls() {
        let events = replay(FIXTURE.len()).await;
        assert_eq!(
            events,
            vec![
                StreamEvent::ReasoningDelta("Let me look.".into()),
                StreamEvent::TextDelta("Reading wörld".into()),
                StreamEvent::TextDelta(" files.".into()),
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
    async fn missing_done_after_finish_reason_is_fine() {
        let events = replay_bytes(fixture_until("data: [DONE]"), FIXTURE.len()).await;
        let events: Vec<_> = events.into_iter().map(|e| e.expect("valid")).collect();
        assert_eq!(events, replay(FIXTURE.len()).await);
    }

    #[tokio::test]
    async fn cut_off_stream_is_an_error_and_runs_no_tools() {
        let events = replay_bytes(fixture_until("\"finish_reason\""), FIXTURE.len()).await;
        assert!(matches!(events.last(), Some(Err(Error::Incomplete))));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ok(StreamEvent::ToolCall(_))))
        );
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
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, Ok(StreamEvent::ToolCall(_))))
        );
    }
}
