//! OpenAI-compatible chat completions over SSE. Works against OpenCode Go and
//! any other endpoint speaking this protocol.

mod sse;
mod wire;

use std::collections::{BTreeMap, VecDeque};

use futures::{FutureExt, Stream, StreamExt, future::BoxFuture, stream::BoxStream};
use nth_protocol::{BoxError, Provider, Request, StreamEvent, ToolCall};

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
}

pub struct ChatClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    session_id: String,
}

impl ChatClient {
    /// `session_id` must stay stable for a conversation: Go routes and caches
    /// prompts on the `x-opencode-session` header.
    pub fn new(base_url: String, api_key: String, model: String, session_id: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
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
            .json(&wire::body(&self.model, request.messages, request.tools))
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Status { status, body });
        }

        let bytes = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(Error::from))
            .boxed();
        Ok(events(bytes).map(|e| e.map_err(BoxError::from)).boxed())
    }
}

impl Provider for ChatClient {
    fn model(&self) -> &str {
        &self.model
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
    pending: VecDeque<StreamEvent>,
    calls: BTreeMap<usize, ToolCall>,
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
        finished: false,
    };
    futures::stream::unfold(state, |mut s| async move {
        loop {
            if let Some(event) = s.pending.pop_front() {
                return Some((Ok(event), s));
            }
            if s.finished {
                return None;
            }
            let parsed = match s.bytes.next().await {
                Some(Ok(chunk)) => s.parser.push(&chunk),
                Some(Err(e)) => Err(e),
                None => Ok(vec![sse::Event::Done]),
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
                    sse::Event::Done => {
                        s.finished = true;
                        let calls = std::mem::take(&mut s.calls);
                        s.pending
                            .extend(calls.into_values().map(StreamEvent::ToolCall));
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
            self.pending.push_back(StreamEvent::ReasoningDelta(text));
        }
        if let Some(text) = delta.content.filter(|t| !t.is_empty()) {
            self.pending.push_back(StreamEvent::TextDelta(text));
        }
        for part in delta.tool_calls {
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
        let chunks = FIXTURE
            .chunks(chunk_size)
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect::<Vec<_>>();
        events(futures::stream::iter(chunks))
            .map(|e| e.expect("fixture is valid"))
            .collect()
            .await
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
}
