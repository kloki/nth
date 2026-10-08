//! What every protocol's stream shares: SSE arrives as lines, each protocol
//! reads its `data:` payloads into its own events, and a payload that does
//! not parse is quoted.

/// How much of an unparseable line an error quotes. Enough to see what the
/// server sent, little enough that the error still fits in the TUI.
pub(crate) const EXCERPT_CHARS: usize = 200;

/// Splits raw response bytes into lines. Network chunks can end anywhere,
/// even inside a UTF-8 character, so bytes are buffered until a full line
/// arrives.
#[derive(Default)]
pub(crate) struct Lines {
    buf: Vec<u8>,
}

impl Lines {
    /// The lines `bytes` complete, without their line endings.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut lines = Vec::new();
        while let Some(end) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=end).collect();
            lines.push(String::from_utf8_lossy(&line).trim_end().to_string());
        }
        lines
    }

    /// What is left once the bytes end. Some servers close the connection
    /// right after the last line, without its newline.
    pub(crate) fn finish(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.buf))
            .trim_end()
            .to_string()
    }
}

/// The payload of a `data:` line. Other fields and comments are none, and
/// so is an empty payload, which some servers send as a heartbeat.
pub(crate) fn data(line: &str) -> Option<&str> {
    let data = line.strip_prefix("data:")?.trim_start();
    (!data.is_empty()).then_some(data)
}

/// Turns raw response bytes into a protocol's events: `Lines` splits them,
/// and `parse` reads one line into whatever events it carries.
pub(crate) struct Parser<F> {
    lines: Lines,
    parse: F,
}

impl<F, I, E> Parser<F>
where
    F: FnMut(&str) -> Result<I, E>,
    I: IntoIterator,
{
    pub(crate) fn new(parse: F) -> Self {
        Self {
            lines: Lines::default(),
            parse,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<I::Item>, E> {
        let mut events = Vec::new();
        for line in self.lines.push(bytes) {
            events.extend((self.parse)(&line)?);
        }
        Ok(events)
    }

    /// Parses what is left once the bytes end.
    pub(crate) fn finish(&mut self) -> Result<Vec<I::Item>, E> {
        Ok((self.parse)(&self.lines.finish())?.into_iter().collect())
    }
}

pub(crate) fn excerpt(data: &str) -> String {
    match data.char_indices().nth(EXCERPT_CHARS) {
        Some((end, _)) => format!("{}…", &data[..end]),
        None => data.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_survive_any_split_even_inside_a_character() {
        let input = "data: wörld\r\n\ndata: x".as_bytes();
        for at in 0..input.len() {
            let mut lines = Lines::default();
            let mut got = lines.push(&input[..at]);
            got.extend(lines.push(&input[at..]));
            got.push(lines.finish());
            assert_eq!(got, ["data: wörld", "", "data: x"], "split at {at}");
        }
    }

    #[test]
    fn the_parser_hands_each_line_to_the_protocol() {
        let mut parser =
            Parser::new(|line: &str| -> Result<Option<usize>, ()> { Ok(data(line).map(str::len)) });
        assert_eq!(
            parser.push(b"data: ab\n: comment\ndata: c").expect("valid"),
            [2]
        );
        assert_eq!(parser.finish().expect("valid"), [1]);
        assert!(parser.finish().expect("valid").is_empty(), "nothing left");
    }

    #[test]
    fn only_non_empty_data_is_a_payload() {
        assert_eq!(data("data: {}"), Some("{}"));
        assert_eq!(data("data:{}"), Some("{}"));
        assert_eq!(data("data:"), None);
        assert_eq!(data("data: "), None);
        assert_eq!(data("event: ping"), None);
        assert_eq!(data(": keep-alive"), None);
    }
}
