//! Every `data:` line is one JSON event whose `type` repeats the `event:`
//! field, so the `event:` lines are ignored. Event and block types this does
//! not know (`ping`, `signature_delta`, ones added later) are skipped.

use serde::Deserialize;

use super::Error;
use crate::sse::{self, data, excerpt};

#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    MessageStart {
        message: Start,
    },
    ContentBlockStart {
        index: usize,
        content_block: Block,
    },
    ContentBlockDelta {
        index: usize,
        delta: Delta,
    },
    MessageDelta {
        delta: Stop,
        usage: Option<Usage>,
    },
    MessageStop,
    Error {
        error: ErrorBody,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, PartialEq, Deserialize)]
pub struct Start {
    pub usage: Option<Usage>,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    /// Its input arrives as `input_json_delta`s; the `input` here is empty.
    ToolUse { id: String, name: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Delta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, PartialEq, Deserialize)]
pub struct Stop {
    pub stop_reason: Option<String>,
}

/// Counts so far. `message_start` has the input, `message_delta` the output
/// and, from some servers, the input again; an absent count is unchanged.
#[derive(Debug, Default, Clone, Copy, PartialEq, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
}

impl Usage {
    pub fn update(&mut self, newer: Usage) {
        let keep = |old: &mut Option<u64>, new: Option<u64>| *old = new.or(*old);
        keep(&mut self.input_tokens, newer.input_tokens);
        keep(&mut self.output_tokens, newer.output_tokens);
        keep(
            &mut self.cache_creation_input_tokens,
            newer.cache_creation_input_tokens,
        );
        keep(
            &mut self.cache_read_input_tokens,
            newer.cache_read_input_tokens,
        );
    }

    /// Everything sent: what was cached or read from the cache is counted
    /// apart from the rest.
    pub fn input(self) -> u64 {
        [
            self.input_tokens,
            self.cache_creation_input_tokens,
            self.cache_read_input_tokens,
        ]
        .into_iter()
        .flatten()
        .sum()
    }

    pub fn output(self) -> u64 {
        self.output_tokens.unwrap_or_default()
    }
}

/// The error object of an `error` event, and of a failed response's body.
#[derive(Debug, PartialEq, Deserialize)]
pub struct ErrorBody {
    #[serde(rename = "type")]
    pub kind: String,
    pub message: String,
}

/// A whole error response: `{"type":"error","error":{...}}`.
#[derive(Deserialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

/// Turns raw response bytes into events.
pub(super) type Parser = sse::Parser<fn(&str) -> Result<Option<Event>, Error>>;

pub(super) fn parser() -> Parser {
    sse::Parser::new(parse_line)
}

fn parse_line(line: &str) -> Result<Option<Event>, Error> {
    let Some(data) = data(line) else {
        return Ok(None);
    };
    match serde_json::from_str(data) {
        Ok(Event::Error { error }) => Err(Error::Provider {
            kind: error.kind,
            message: error.message,
        }),
        Ok(event) => Ok(Some(event)),
        Err(source) => Err(Error::Parse {
            source,
            line: excerpt(data),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bad_json_is_an_error() {
        let mut parser = parser();
        assert!(matches!(
            parser.push(b"data: {nope\n"),
            Err(Error::Parse { .. })
        ));
    }

    #[test]
    fn unknown_events_and_blocks_are_skipped() {
        let mut parser = parser();
        let events = parser
            .push(
                b"event: ping\ndata: {\"type\":\"ping\"}\n\n\
                  data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"x\"}}\n\
                  data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"s\"}}\n",
            )
            .expect("valid");
        assert_eq!(
            events,
            vec![
                Event::Other,
                Event::ContentBlockStart {
                    index: 0,
                    content_block: Block::Other
                },
                Event::ContentBlockDelta {
                    index: 0,
                    delta: Delta::Other
                },
            ]
        );
    }

    #[test]
    fn an_error_event_is_an_error_of_its_kind() {
        let mut parser = parser();
        let error = parser
            .push(b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n")
            .expect_err("error event");
        assert_eq!(
            error.to_string(),
            "provider error overloaded_error: Overloaded"
        );
    }

    #[test]
    fn later_usage_updates_only_what_it_counts() {
        let mut usage = Usage {
            input_tokens: Some(10),
            output_tokens: Some(1),
            cache_creation_input_tokens: Some(0),
            cache_read_input_tokens: Some(500),
        };
        usage.update(Usage {
            output_tokens: Some(42),
            ..Usage::default()
        });
        assert_eq!((usage.input(), usage.output()), (510, 42));
    }
}
