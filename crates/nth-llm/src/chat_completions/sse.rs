use serde::Deserialize;

use super::Error;

#[derive(Debug, PartialEq)]
pub enum Event {
    Delta(Delta),
    Finish(String),
    Done,
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
}

fn parse_line(line: &str, events: &mut Vec<Event>) -> Result<(), Error> {
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(());
    };
    let data = data.trim_start();
    if data == "[DONE]" {
        events.push(Event::Done);
        return Ok(());
    }
    let chunk: Chunk = serde_json::from_str(data).map_err(|source| Error::Parse {
        source,
        line: data.to_string(),
    })?;
    if let Some(error) = chunk.error {
        let message = match error.get("message").and_then(|m| m.as_str()) {
            Some(message) => message.to_string(),
            None => error.to_string(),
        };
        return Err(Error::Provider(message));
    }
    let Some(choice) = chunk.choices.into_iter().flatten().next() else {
        return Ok(());
    };
    events.extend(choice.delta.map(Event::Delta));
    events.extend(choice.finish_reason.map(Event::Finish));
    Ok(())
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
    fn ignores_comments_and_usage_chunks() {
        let mut parser = Parser::default();
        let events = parser
            .push(b": ping\n\ndata: {\"choices\":[],\"usage\":{}}\n\ndata: [DONE]\n")
            .expect("valid");
        assert_eq!(events, vec![Event::Done]);
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
