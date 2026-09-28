use serde::Deserialize;

use super::Error;

#[derive(Debug, PartialEq)]
pub enum Event {
    Delta(String),
    Done,
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
    delta: Delta,
}

#[derive(Deserialize)]
struct Delta {
    content: Option<String>,
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
    let content = chunk
        .choices
        .into_iter()
        .next()
        .and_then(|c| c.delta.content)
        .filter(|c| !c.is_empty());
    Ok(content.map(Event::Delta))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../../tests/fixtures/go_stream.sse");

    fn collect(chunk_size: usize) -> Vec<Event> {
        let mut parser = Parser::default();
        FIXTURE
            .chunks(chunk_size)
            .flat_map(|c| parser.push(c).expect("fixture is valid"))
            .collect()
    }

    #[test]
    fn parses_fixture_whole() {
        let events = collect(FIXTURE.len());
        assert_eq!(
            events,
            vec![
                Event::Delta("Hello".into()),
                Event::Delta(" wörld".into()),
                Event::Delta("!".into()),
                Event::Done,
            ]
        );
    }

    #[test]
    fn chunk_boundaries_do_not_matter() {
        let whole = collect(FIXTURE.len());
        for size in 1..16 {
            assert_eq!(collect(size), whole, "chunk size {size}");
        }
    }

    #[test]
    fn bad_json_is_an_error() {
        let mut parser = Parser::default();
        assert!(parser.push(b"data: {nope\n").is_err());
    }
}
