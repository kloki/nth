use serde::Deserialize;

use super::Error;

#[derive(Debug, PartialEq)]
pub enum Event {
    Delta(Delta),
    Done,
}

#[derive(Debug, Default, PartialEq, Deserialize)]
pub struct Delta {
    pub content: Option<String>,
    /// DeepSeek and Kimi use `reasoning_content`, others plain `reasoning`.
    #[serde(alias = "reasoning")]
    pub reasoning_content: Option<String>,
    #[serde(default)]
    pub tool_calls: Vec<ToolCallDelta>,
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
    #[serde(default)]
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    delta: Option<Delta>,
}

impl Parser {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<Event>, Error> {
        self.buf.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(end) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=end).collect();
            let line = String::from_utf8_lossy(&line);
            if let Some(event) = parse_line(line.trim_end())? {
                events.push(event);
            }
        }
        Ok(events)
    }
}

fn parse_line(line: &str) -> Result<Option<Event>, Error> {
    let Some(data) = line.strip_prefix("data:") else {
        return Ok(None);
    };
    let data = data.trim_start();
    if data == "[DONE]" {
        return Ok(Some(Event::Done));
    }
    let chunk: Chunk = serde_json::from_str(data).map_err(|source| Error::Parse {
        source,
        line: data.to_string(),
    })?;
    Ok(chunk
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.delta)
        .map(Event::Delta))
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
}
