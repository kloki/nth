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

/// `prompt_tokens` counts everything sent, what was read from the cache
/// included. Servers report the cached part in one of three places.
#[derive(Debug, Default, PartialEq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    /// OpenAI's place, which most compatible servers copy.
    pub prompt_tokens_details: Option<PromptTokensDetails>,
    /// DeepSeek's.
    pub prompt_cache_hit_tokens: Option<u64>,
    /// Moonshot's, at the top level.
    pub cached_tokens: Option<u64>,
}

#[derive(Debug, Default, PartialEq, Deserialize)]
pub struct PromptTokensDetails {
    pub cached_tokens: Option<u64>,
}

impl Usage {
    /// What was read from the cache, wherever the server put it; `None`
    /// when it put it nowhere.
    pub fn cache_read(&self) -> Option<u64> {
        self.prompt_tokens_details
            .as_ref()
            .and_then(|details| details.cached_tokens)
            .or(self.prompt_cache_hit_tokens)
            .or(self.cached_tokens)
    }
}

#[derive(Debug, Default, PartialEq, Deserialize)]
#[serde(from = "RawDelta")]
pub struct Delta {
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize)]
struct RawDelta {
    content: Option<Content>,
    /// DeepSeek and Kimi use `reasoning_content`, others plain `reasoning`.
    #[serde(alias = "reasoning")]
    reasoning_content: Option<String>,
    /// Some servers send `"tool_calls": null` on plain text deltas.
    tool_calls: Option<Vec<ToolCallDelta>>,
}

/// Mistral sends content as a list of typed chunks rather than a string,
/// with its reasoning in `thinking` chunks among them.
#[derive(Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Chunks(Vec<ContentChunk>),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum ContentChunk {
    Text {
        text: String,
    },
    Thinking {
        thinking: Vec<ContentChunk>,
    },
    /// Images, references and whatever comes next carry nothing to show.
    #[serde(other)]
    Other,
}

impl ContentChunk {
    /// Appends the chunk's text to `text`, or to `reasoning` when it is thinking.
    fn collect(self, text: &mut String, reasoning: &mut String) {
        match self {
            ContentChunk::Text { text: t } => text.push_str(&t),
            ContentChunk::Thinking { thinking } => {
                for chunk in thinking {
                    chunk.collect(reasoning, &mut String::new());
                }
            }
            ContentChunk::Other => {}
        }
    }
}

impl From<RawDelta> for Delta {
    fn from(raw: RawDelta) -> Self {
        let mut reasoning_content = raw.reasoning_content;
        let content = match raw.content {
            None => None,
            Some(Content::Text(text)) => Some(text),
            Some(Content::Chunks(chunks)) => {
                let (mut text, mut reasoning) = (String::new(), String::new());
                for chunk in chunks {
                    chunk.collect(&mut text, &mut reasoning);
                }
                if !reasoning.is_empty() {
                    reasoning_content
                        .get_or_insert_default()
                        .push_str(&reasoning);
                }
                (!text.is_empty()).then_some(text)
            }
        };
        Delta {
            content,
            reasoning_content,
            tool_calls: raw.tool_calls,
        }
    }
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
                    completion_tokens: 8,
                    ..Usage::default()
                }),
                Event::Done
            ]
        );
    }

    #[test]
    fn reads_the_cached_count_wherever_the_server_puts_it() {
        let read = |usage: &str| {
            let line = format!("data: {{\"choices\":[],\"usage\":{usage}}}\n");
            match parser().push(line.as_bytes()).expect("valid").as_slice() {
                [Event::Usage(usage)] => usage.cache_read(),
                other => panic!("not one usage: {other:?}"),
            }
        };
        assert_eq!(
            read(r#"{"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":80}}"#),
            Some(80)
        );
        assert_eq!(
            read(
                r#"{"prompt_tokens":100,"prompt_cache_hit_tokens":70,"prompt_cache_miss_tokens":30}"#
            ),
            Some(70)
        );
        assert_eq!(
            read(r#"{"prompt_tokens":100,"cached_tokens":60}"#),
            Some(60)
        );
        assert_eq!(
            read(r#"{"prompt_tokens":100,"prompt_tokens_details":null}"#),
            None
        );
        assert_eq!(read(r#"{"prompt_tokens":100}"#), None);
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

    fn delta(content: &str) -> Delta {
        let line = format!("data: {{\"choices\":[{{\"delta\":{{\"content\":{content}}}}}]}}\n");
        let mut events = parser().push(line.as_bytes()).expect("valid");
        match (events.pop(), events.is_empty()) {
            (Some(Event::Delta(delta)), true) => delta,
            other => panic!("expected one delta, got {other:?}"),
        }
    }

    #[test]
    fn mistral_chunks_split_into_text_and_reasoning() {
        assert_eq!(
            delta(
                r#"[{"type":"thinking","thinking":[{"type":"text","text":"hmm"}],"closed":true},{"type":"text","text":"hi"}]"#
            ),
            Delta {
                content: Some("hi".into()),
                reasoning_content: Some("hmm".into()),
                tool_calls: None,
            }
        );
    }

    #[test]
    fn mistral_text_chunks_are_content() {
        assert_eq!(
            delta(r#"[{"type":"text","text":"a"},{"type":"text","text":"b"}]"#),
            Delta {
                content: Some("ab".into()),
                ..Delta::default()
            }
        );
    }

    #[test]
    fn unknown_chunks_are_dropped() {
        assert_eq!(
            delta(r#"[{"type":"image_url","image_url":"x"}]"#),
            Delta::default()
        );
    }
}
