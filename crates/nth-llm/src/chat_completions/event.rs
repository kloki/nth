//! The SSE parser is line-based: every `data:` line is one JSON document.
//! `event:` fields and multi-line `data:` are legal SSE but ignored, because
//! every OpenAI-compatible server today sends one chunk per `data:` line.

use serde::Deserialize;

use super::Error;
use crate::sse::{self, data, excerpt};

#[derive(Debug, PartialEq)]
pub enum Event {
    Delta(Delta),
    Finish(String),
    Usage(Usage),
    Done,
}

#[derive(Debug, Default, PartialEq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

#[derive(Debug, Default, PartialEq, Deserialize)]
pub struct Delta {
    pub content: Option<String>,
    /// DeepSeek and Kimi use `reasoning_content`, others plain `reasoning`.
    #[serde(alias = "reasoning")]
    pub reasoning_content: Option<String>,
    /// Some servers send `"tool_calls": null` on plain text deltas.
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

/// One fragment of a tool call. The first fragment for an `index` carries
/// the id and name, later ones append to the arguments string.
#[derive(Debug, PartialEq, Deserialize)]
pub struct ToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    pub function: Option<FunctionDelta>,
}

#[derive(Debug, PartialEq, Deserialize)]
pub struct FunctionDelta {
    pub name: Option<String>,
    pub arguments: Option<String>,
}

/// Turns raw response bytes into events.
pub(super) type Parser = sse::Parser<fn(&str) -> Result<Vec<Event>, Error>>;

pub(super) fn parser() -> Parser {
    sse::Parser::new(parse_line)
}

#[derive(Deserialize)]
struct Chunk {
    choices: Option<Vec<Choice>>,
    /// Gateways such as OpenRouter report rate limits and overload in-stream,
    /// after the 200 status has already been sent.
    error: Option<serde_json::Value>,
    /// Sent in a chunk of its own, with no choices, near the end.
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    delta: Option<Delta>,
    finish_reason: Option<String>,
}

fn parse_line(line: &str) -> Result<Vec<Event>, Error> {
    let mut events = Vec::new();
    let Some(data) = data(line) else {
        return Ok(events);
    };
    if data == "[DONE]" {
        events.push(Event::Done);
        return Ok(events);
    }
    let chunk: Chunk = serde_json::from_str(data).map_err(|source| Error::Parse {
        source,
        line: excerpt(data),
    })?;
    if let Some(error) = chunk.error {
        let message = match error.get("message").and_then(|m| m.as_str()) {
            Some(message) => message.to_string(),
            None => error.to_string(),
        };
        return Err(Error::Provider {
            message,
            status: error_status(&error),
        });
    }
    events.extend(chunk.usage.map(Event::Usage));
    let Some(choice) = chunk.choices.into_iter().flatten().next() else {
        return Ok(events);
    };
    events.extend(choice.delta.map(Event::Delta));
    events.extend(choice.finish_reason.map(Event::Finish));
    Ok(events)
}

/// The HTTP status an in-stream error stands for, so that a rate limit or
/// an overload reported this way is retried like one sent as the status.
/// Gateways put it in `code` or `status`, as a number or a numeric string;
/// a symbolic code such as `"rate_limit_exceeded"` says nothing usable.
fn error_status(error: &serde_json::Value) -> Option<reqwest::StatusCode> {
    ["code", "status"]
        .into_iter()
        .filter_map(|key| error.get(key))
        .find_map(|value| match value {
            serde_json::Value::Number(n) => n.as_u64().and_then(|n| u16::try_from(n).ok()),
            serde_json::Value::String(s) => s.parse().ok(),
            _ => None,
        })
        .and_then(|code| reqwest::StatusCode::from_u16(code).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::EXCERPT_CHARS;

    #[test]
    fn bad_json_is_an_error() {
        let mut parser = parser();
        assert!(parser.push(b"data: {nope\n").is_err());
    }

    #[test]
    fn a_parse_error_quotes_only_the_start_of_the_line() {
        let mut parser = parser();
        let long = format!("data: {{{}\n", "x".repeat(1000));
        let err = parser.push(long.as_bytes()).expect_err("bad json");
        let Error::Parse { line, .. } = err else {
            panic!("expected a parse error, got {err}");
        };
        assert_eq!(line.chars().count(), EXCERPT_CHARS + 1, "{line}");
        assert!(line.ends_with('…'));
    }

    #[test]
    fn finish_parses_a_last_line_without_a_newline() {
        let mut parser = parser();
        assert_eq!(parser.push(b"data: [DONE]").expect("valid"), vec![]);
        assert_eq!(parser.finish().expect("valid"), vec![Event::Done]);
        assert_eq!(parser.finish().expect("valid"), vec![], "nothing left");
    }

    #[test]
    fn empty_data_is_a_heartbeat() {
        let mut parser = parser();
        let events = parser
            .push(b"data:\n\ndata: \n\ndata: [DONE]\n")
            .expect("valid");
        assert_eq!(events, vec![Event::Done]);
    }

    #[test]
    fn ignores_comments_and_reads_usage_chunks() {
        let mut parser = parser();
        let events = parser
            .push(b": ping\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":120,\"completion_tokens\":8,\"total_tokens\":128}}\n\ndata: [DONE]\n")
            .expect("valid");
        assert_eq!(
            events,
            vec![
                Event::Usage(Usage {
                    prompt_tokens: 120,
                    completion_tokens: 8
                }),
                Event::Done
            ]
        );
    }

    #[test]
    fn in_stream_error_is_an_error() {
        let mut parser = parser();
        let err = parser
            .push(b"data: {\"error\":{\"message\":\"rate limited\",\"code\":429}}\n")
            .expect_err("error chunk");
        assert!(err.to_string().contains("rate limited"), "{err}");
    }

    #[test]
    fn in_stream_error_carries_its_status() {
        let status = |chunk: &[u8]| match parser().push(chunk) {
            Err(Error::Provider { status, .. }) => status,
            other => panic!("expected a provider error, got {other:?}"),
        };
        assert_eq!(
            status(b"data: {\"error\":{\"message\":\"slow down\",\"code\":429}}\n"),
            Some(reqwest::StatusCode::TOO_MANY_REQUESTS)
        );
        assert_eq!(
            status(b"data: {\"error\":{\"message\":\"overloaded\",\"status\":\"503\"}}\n"),
            Some(reqwest::StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(
            status(b"data: {\"error\":{\"message\":\"nope\",\"code\":\"invalid_request\"}}\n"),
            None
        );
        assert_eq!(status(b"data: {\"error\":\"plain string\"}\n"), None);
    }

    #[test]
    fn null_tool_calls_parse() {
        let mut parser = parser();
        let events = parser
            .push(b"data: {\"choices\":[{\"delta\":{\"content\":\"hi\",\"tool_calls\":null},\"finish_reason\":null}]}\n")
            .expect("valid");
        assert_eq!(
            events,
            vec![Event::Delta(Delta {
                content: Some("hi".into()),
                ..Delta::default()
            })]
        );
    }
}
