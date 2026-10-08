//! What every protocol's stream shares: SSE arrives as lines, each protocol
//! reads its `data:` payloads, and a payload that does not parse is quoted.

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
    fn only_non_empty_data_is_a_payload() {
        assert_eq!(data("data: {}"), Some("{}"));
        assert_eq!(data("data:{}"), Some("{}"));
        assert_eq!(data("data:"), None);
        assert_eq!(data("data: "), None);
        assert_eq!(data("event: ping"), None);
        assert_eq!(data(": keep-alive"), None);
    }
}
