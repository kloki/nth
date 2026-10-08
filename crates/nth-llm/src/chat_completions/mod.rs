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
use nth_protocol::{BoxError, Request, Retry, StreamEvent, ToolCall, Usage};

use crate::http::{STREAM_IDLE_TIMEOUT, USER_AGENT, retry_after, retryable, transient};

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

    pub(crate) async fn stream(
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
    use crate::testing::{chunks, runs_no_tools, until};

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/go_stream.sse");

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

    #[test]
    fn only_transient_errors_are_retryable() {
        let boxed = |error: Error| -> BoxError { Box::new(error) };

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
