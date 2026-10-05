//! OpenAI-compatible chat completions over SSE. Works against OpenCode Go and
//! any other endpoint speaking this protocol.

mod models;
mod sse;
mod wire;

use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

use futures::{FutureExt, Stream, StreamExt, future::BoxFuture, stream::BoxStream};
use nth_protocol::{BoxError, ModelInfo, Provider, Request, Retry, StreamEvent, ToolCall, Usage};

const USER_AGENT: &str = concat!("nth/", env!("CARGO_PKG_VERSION"));

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
}

impl ChatClient {
    pub fn new(base_url: String, api_key: String) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
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
            .header("x-opencode-session", request.session_id)
            .json(&wire::body(
                request.model,
                request.effort,
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

/// Turns a non-2xx response into an error that carries the server's body
/// and its `Retry-After`, if any.
async fn success(response: reqwest::Response) -> Result<reqwest::Response, Error> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let retry_after = retry_after(response.headers());
    let body = response.text().await.unwrap_or_default();
    Err(Error::Status {
        status,
        body,
        retry_after,
    })
}

/// How long the server asked to wait, from `Retry-After-Ms` or the usual
/// `Retry-After` in seconds; the HTTP-date form is not supported.
fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let seconds = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|seconds| *seconds >= 0.0)
    };
    if let Some(ms) = seconds("retry-after-ms") {
        return Some(Duration::from_secs_f64(ms / 1000.0));
    }
    seconds("retry-after").map(Duration::from_secs_f64)
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

    fn retry(&self, error: &BoxError) -> Option<Retry> {
        let error = error.downcast_ref::<Error>()?;
        match error {
            // A rate limit or a transient server failure; the server may
            // have said how long to wait.
            Error::Status {
                status,
                retry_after,
                ..
            } if *status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() => {
                Some(Retry {
                    after: *retry_after,
                })
            }
            // A connection error, including one part-way through the stream.
            Error::Http(_) => Some(Retry { after: None }),
            // The stream ended before the reply did.
            Error::Incomplete => Some(Retry { after: None }),
            _ => None,
        }
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
                    sse::Event::Usage(usage) => {
                        s.pending.push_back(Ok(StreamEvent::Usage(Usage {
                            input: usage.prompt_tokens,
                            output: usage.completion_tokens,
                        })))
                    }
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

    fn client() -> ChatClient {
        ChatClient::new("http://localhost".into(), "key".into())
    }

    #[test]
    fn only_transient_errors_are_retryable() {
        let boxed = |error: Error| -> BoxError { Box::new(error) };

        let limited = boxed(Error::Status {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            body: String::new(),
            retry_after: Some(Duration::from_secs(7)),
        });
        assert_eq!(
            client().retry(&limited),
            Some(Retry {
                after: Some(Duration::from_secs(7))
            })
        );

        let overloaded = boxed(Error::Status {
            status: reqwest::StatusCode::SERVICE_UNAVAILABLE,
            body: String::new(),
            retry_after: None,
        });
        assert_eq!(client().retry(&overloaded), Some(Retry { after: None }));

        let rejected = boxed(Error::Status {
            status: reqwest::StatusCode::BAD_REQUEST,
            body: String::new(),
            retry_after: None,
        });
        assert_eq!(client().retry(&rejected), None);
        assert_eq!(
            client().retry(&boxed(Error::Incomplete)),
            Some(Retry { after: None })
        );
        assert_eq!(client().retry(&boxed(Error::Truncated)), None);
    }

    #[test]
    fn retry_after_reads_seconds_or_milliseconds() {
        let header = |name: &'static str, value: &str| {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(name, value.parse().expect("header"));
            headers
        };

        assert_eq!(
            retry_after(&header("retry-after", "3")),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            retry_after(&header("retry-after-ms", "1500")),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(
            retry_after(&header("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")),
            None,
            "the HTTP-date form is not parsed"
        );
        assert_eq!(retry_after(&reqwest::header::HeaderMap::new()), None);
    }
}
