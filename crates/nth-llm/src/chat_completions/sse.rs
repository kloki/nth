//! The SSE parser is line-based: every `data:` line is one JSON document.
//! `event:` fields and multi-line `data:` are legal SSE but ignored, because
//! every OpenAI-compatible server today sends one chunk per `data:` line.

use serde::Deserialize;

use super::Error;

/// How much of an unparseable line an error quotes. Enough to see what the
/// server sent, little enough that the error still fits in the TUI.
const EXCERPT_CHARS: usize = 200;

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

/// Turns raw response bytes into SSE events. Network chunks can end anywhere,
/// even inside a UTF-8 character, so bytes are buffered until a full line arrives.
#[derive(Default)]
pub struct Parser {
    buf: Vec<u8>,
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

impl Parser {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Event>, Error> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            parse_line(line.trim_end(), &mut events)?;
        }
        Ok(events)
    }

    /// Parses what is left once the bytes end. Some servers close the
    /// connection right after the last line, without its newline.
    pub fn finish(&mut self) -> Result<Vec<Event>, Error> {
        let line = String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned();
        let mut events = Vec::new();
        parse_line(line.trim_end(), &mut events)?;
        Ok(events)
    }
}

fn parse_line(line: &str, events: &mut Vec<Event>) -> Result<(), Error> {
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(());
    };
    let data = data.trim_start();
    // SSE allows an empty data field; some servers send one as a heartbeat.
    if data.is_empty() {
        return Ok(());
    }
    if data == "[DONE]" {
        events.push(Event::Done);
        return Ok(());
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
        return Ok(());
    };
    events.extend(choice.delta.map(Event::Delta));
    events.extend(choice.finish_reason.map(Event::Finish));
    Ok(())
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

fn excerpt(data: &str) -> String {
    match data.char_indices().nth(EXCERPT_CHARS) {
        Some((end, _)) => format!("{}…", &data[..end]),
        None => data.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_json_is_an_error() {
        let mut parser = Parser::default();
        assert!(parser.push(b"data: {nope\n").is_err());
    }

    #[test]
    fn a_parse_error_quotes_only_the_start_of_the_line() {
        let mut parser = Parser::default();
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
        let mut parser = Parser::default();
        assert_eq!(parser.push(b"data: [DONE]").expect("valid"), vec![]);
        assert_eq!(parser.finish().expect("valid"), vec![Event::Done]);
        assert_eq!(parser.finish().expect("valid"), vec![], "nothing left");
    }

    #[test]
    fn empty_data_is_a_heartbeat() {
        let mut parser = Parser::default();
        let events = parser
            .push(b"data:\n\ndata: \n\ndata: [DONE]\n")
            .expect("valid");
        assert_eq!(events, vec![Event::Done]);
    }

    #[test]
    fn ignores_comments_and_reads_usage_chunks() {
        let mut parser = Parser::default();
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
        let mut parser = Parser::default();
        let err = parser
            .push(b"data: {\"error\":{\"message\":\"rate limited\",\"code\":429}}\n")
            .expect_err("error chunk");
        assert!(err.to_string().contains("rate limited"), "{err}");
    }

    #[test]
    fn in_stream_error_carries_its_status() {
        let status = |chunk: &[u8]| match Parser::default().push(chunk) {
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
        let mut parser = Parser::default();
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
