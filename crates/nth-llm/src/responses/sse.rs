//! Every `data:` line is one JSON event whose `type` repeats the `event:`
//! field, so the `event:` lines are ignored. Events and items this does not
//! know (`response.created`, `response.function_call_arguments.delta`, ones
//! added later) are skipped.

use serde::Deserialize;

use super::Error;
use crate::sse::{Lines, data, excerpt};

#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    #[serde(rename = "response.output_text.delta")]
    TextDelta { delta: String },
    /// OpenAI's models stream a summary of their reasoning, open models
    /// the reasoning itself.
    #[serde(
        rename = "response.reasoning_summary_text.delta",
        alias = "response.reasoning_text.delta"
    )]
    ReasoningDelta { delta: String },
    /// A summary comes in parts, each its own paragraph.
    #[serde(rename = "response.reasoning_summary_part.added")]
    SummaryPart { summary_index: usize },
    /// A finished output item; a tool call arrives whole in one.
    #[serde(rename = "response.output_item.done")]
    ItemDone { output_index: usize, item: Item },
    #[serde(rename = "response.completed")]
    Completed { response: Response },
    /// Stopped early, at the output limit or by a content filter.
    #[serde(rename = "response.incomplete")]
    Incomplete { response: Response },
    #[serde(rename = "response.failed")]
    Failed { response: Response },
    #[serde(rename = "error")]
    Error(ErrorEvent),
    #[serde(other)]
    Other,
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Item {
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(other)]
    Other,
}

/// The response as the closing events carry it, with only what is read.
#[derive(Debug, PartialEq, Deserialize)]
pub struct Response {
    pub usage: Option<Usage>,
    pub incomplete_details: Option<IncompleteDetails>,
    pub error: Option<ErrorBody>,
}

#[derive(Debug, PartialEq, Deserialize)]
pub struct IncompleteDetails {
    pub reason: Option<String>,
}

/// `input_tokens` counts everything sent, what was read from the cache
/// included, which is what `nth_protocol::Usage::input` wants.
#[derive(Debug, Default, Clone, Copy, PartialEq, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

/// The error object of a failed response, and of a failed request's body.
/// OpenAI names its kind `code`, OpenCode Zen `type`.
#[derive(Debug, PartialEq, Deserialize)]
pub struct ErrorBody {
    pub code: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub message: String,
}

impl ErrorBody {
    fn into_error(self) -> Error {
        Error::Provider {
            kind: self.code.or(self.kind).unwrap_or_default(),
            message: self.message,
        }
    }
}

/// An `error` event: the error's fields at the top, as OpenAI sends it, or
/// nested in `error`, as a proxy may.
#[derive(Debug, PartialEq, Deserialize)]
pub struct ErrorEvent {
    pub code: Option<String>,
    pub message: Option<String>,
    pub error: Option<ErrorBody>,
}

/// A whole error response: `{"error":{...}}`.
#[derive(Deserialize)]
pub struct ErrorResponse {
    pub error: ErrorBody,
}

/// Turns raw response bytes into events.
#[derive(Default)]
pub struct Parser {
    lines: Lines,
}

impl Parser {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Event>, Error> {
        let mut events = Vec::new();
        for line in self.lines.push(bytes) {
            events.extend(parse_line(&line)?);
        }
        Ok(events)
    }

    /// Parses what is left once the bytes end.
    pub fn finish(&mut self) -> Result<Vec<Event>, Error> {
        Ok(parse_line(&self.lines.finish())?.into_iter().collect())
    }
}

fn parse_line(line: &str) -> Result<Option<Event>, Error> {
    let Some(data) = data(line) else {
        return Ok(None);
    };
    match serde_json::from_str(data) {
        Ok(Event::Error(ErrorEvent {
            error: Some(error), ..
        })) => Err(error.into_error()),
        Ok(Event::Error(ErrorEvent { code, message, .. })) => Err(Error::Provider {
            kind: code.unwrap_or_default(),
            message: message.unwrap_or_default(),
        }),
        Ok(Event::Failed { response }) => Err(match response.error {
            Some(error) => error.into_error(),
            None => Error::Provider {
                kind: String::new(),
                message: "response failed".into(),
            },
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
        let mut parser = Parser::default();
        assert!(matches!(
            parser.push(b"data: {nope\n"),
            Err(Error::Parse { .. })
        ));
    }

    #[test]
    fn unknown_events_and_items_are_skipped() {
        let mut parser = Parser::default();
        let events = parser
            .push(
                b"event: response.created\ndata: {\"type\":\"response.created\",\"response\":{}}\n\n\
                  data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"summary\":[]}}\n\
                  data: {\"type\":\"response.reasoning_text.delta\",\"delta\":\"hm\"}\n",
            )
            .expect("valid");
        assert_eq!(
            events,
            vec![
                Event::Other,
                Event::ItemDone {
                    output_index: 0,
                    item: Item::Other
                },
                Event::ReasoningDelta { delta: "hm".into() },
            ]
        );
    }

    #[test]
    fn error_events_and_failed_responses_are_errors_of_their_kind() {
        let error = |line: &[u8]| {
            Parser::default()
                .push(line)
                .expect_err("an error")
                .to_string()
        };
        assert_eq!(
            error(b"data: {\"type\":\"error\",\"code\":\"rate_limit_exceeded\",\"message\":\"Slow down\"}\n"),
            "provider error rate_limit_exceeded: Slow down"
        );
        assert_eq!(
            error(b"data: {\"type\":\"error\",\"error\":{\"type\":\"server_error\",\"message\":\"Upstream failed\"}}\n"),
            "provider error server_error: Upstream failed"
        );
        assert_eq!(
            error(b"data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_error\",\"message\":\"Oops\"}}}\n"),
            "provider error server_error: Oops"
        );
    }
}
