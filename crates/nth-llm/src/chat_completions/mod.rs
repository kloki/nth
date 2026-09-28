//! OpenAI-compatible chat completions over SSE. Works against OpenCode Go and
//! any other endpoint speaking this protocol.

mod sse;

use std::collections::VecDeque;

use futures::{Stream, StreamExt, stream::BoxStream};
use serde::Serialize;

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

#[derive(Debug, Clone, Serialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    messages: &'a [Message],
    stream: bool,
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

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Streams the assistant's text deltas. The stream ends at `[DONE]`.
    pub async fn stream(
        &self,
        messages: &[Message],
    ) -> Result<BoxStream<'static, Result<String, Error>>, Error> {
        let response = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header("x-opencode-session", &self.session_id)
            .json(&Request {
                model: &self.model,
                messages,
                stream: true,
            })
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Status { status, body });
        }

        Ok(deltas(response.bytes_stream().boxed()).boxed())
    }
}

struct State {
    bytes: BoxStream<'static, reqwest::Result<bytes::Bytes>>,
    parser: sse::Parser,
    pending: VecDeque<sse::Event>,
    done: bool,
}

fn deltas(
    bytes: BoxStream<'static, reqwest::Result<bytes::Bytes>>,
) -> impl Stream<Item = Result<String, Error>> {
    let state = State {
        bytes,
        parser: sse::Parser::default(),
        pending: VecDeque::new(),
        done: false,
    };
    futures::stream::unfold(state, |mut s| async move {
        loop {
            if s.done {
                return None;
            }
            match s.pending.pop_front() {
                Some(sse::Event::Delta(text)) => return Some((Ok(text), s)),
                Some(sse::Event::Done) => return None,
                None => {}
            }
            match s.bytes.next().await {
                Some(Ok(chunk)) => match s.parser.push(&chunk) {
                    Ok(events) => s.pending.extend(events),
                    Err(e) => {
                        s.done = true;
                        return Some((Err(e), s));
                    }
                },
                Some(Err(e)) => {
                    s.done = true;
                    return Some((Err(e.into()), s));
                }
                None => return None,
            }
        }
    })
}
