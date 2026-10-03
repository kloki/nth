//! The tool calls still running, each with what it has produced so far,
//! pinned under the transcript until the call finishes.

use std::path::Path;

use nth_protocol::{Event, ToolCall};
use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

use crate::theme::{BAR, dim};

/// The most a call's body shows.
const BODY_LINES: usize = 10;

#[derive(Default)]
pub struct Live {
    /// In the order they started.
    calls: Vec<Running>,
}

struct Running {
    call: ToolCall,
    body: Vec<String>,
}

/// Which lines of a call's output are worth seeing.
#[derive(PartialEq)]
enum Keep {
    /// The top of a file: where it starts says what it is.
    First,
    /// The end of a command's output: where it has got to.
    Last,
}

fn keep(tool: &str) -> Keep {
    match tool {
        "bash" => Keep::Last,
        _ => Keep::First,
    }
}

impl Live {
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::ToolStarted(call) => {
                let mut running = Running {
                    call: call.clone(),
                    body: Vec::new(),
                };
                // A write's content is in its arguments; it streams nothing.
                if call.name == "write" {
                    let args = serde_json::from_str::<serde_json::Value>(&call.arguments);
                    if let Some(content) = args.ok().as_ref().and_then(|a| a["content"].as_str()) {
                        running.push(content);
                    }
                }
                self.calls.push(running);
            }
            Event::ToolOutput { call_id, text } => {
                if let Some(running) = self.calls.iter_mut().find(|r| r.call.id == *call_id) {
                    running.push(text);
                }
            }
            Event::ToolFinished { call, .. } => self.calls.retain(|r| r.call.id != call.id),
            Event::TextDelta(_) | Event::ReasoningDelta(_) => {}
        }
    }

    /// Drops every call, for a turn that ended without finishing them.
    pub fn clear(&mut self) {
        self.calls.clear();
    }

    /// At most `rows` lines, starting with a blank one. When the calls do
    /// not fit, the oldest shrink to their header first.
    pub fn lines(&self, cwd: &Path, rows: usize) -> Vec<Line<'static>> {
        if self.calls.is_empty() || rows < 2 {
            return Vec::new();
        }
        let room = rows - 1;
        let mut heights: Vec<usize> = self.calls.iter().map(|r| 1 + r.body.len()).collect();
        for i in 0..heights.len() {
            if heights.iter().sum::<usize>() <= room {
                break;
            }
            heights[i] = 1;
        }

        let bar = Style::new().fg(Color::Yellow);
        let mut lines = vec![Line::default()];
        for (running, height) in self.calls.iter().zip(heights) {
            lines.push(Line::from(vec![
                Span::styled(BAR, bar),
                Span::styled("▸ ", dim()),
                Span::styled(
                    format!("{:<6} ", running.call.name),
                    Style::new().fg(Color::Cyan),
                ),
                Span::styled(running.call.summary(cwd), dim()),
            ]));
            for text in running.body.iter().take(height - 1) {
                lines.push(Line::from(vec![
                    Span::styled(BAR, bar),
                    Span::raw(text.clone()),
                ]));
            }
        }
        // Still too many headers: the newest calls matter most.
        let extra = lines.len().saturating_sub(rows);
        if extra > 0 {
            lines.drain(1..1 + extra);
        }
        lines
    }
}

impl Running {
    fn push(&mut self, text: &str) {
        let lines = text.lines().map(|line| line.replace('\t', "    "));
        match keep(&self.call.name) {
            Keep::First => {
                let room = BODY_LINES.saturating_sub(self.body.len());
                self.body.extend(lines.take(room));
            }
            Keep::Last => {
                self.body.extend(lines);
                let extra = self.body.len().saturating_sub(BODY_LINES);
                self.body.drain(..extra);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::{Event, ToolCall};

    use super::*;

    fn call(id: &str, name: &str, arguments: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    fn output(id: &str, text: &str) -> Event {
        Event::ToolOutput {
            call_id: id.into(),
            text: text.into(),
        }
    }

    fn numbered(range: std::ops::RangeInclusive<usize>) -> String {
        range.map(|i| format!("{i}\n")).collect()
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn a_call_shows_until_it_finishes() {
        let mut live = Live::default();
        let bash = call("1", "bash", r#"{"command":"cargo test"}"#);
        live.apply(&Event::ToolStarted(bash.clone()));
        live.apply(&output("1", "compiling\n"));
        live.apply(&output("2", "not this call\n"));

        assert_eq!(
            text(&live.lines(Path::new("/repo"), 20)),
            ["", "▎ ▸ bash   cargo test", "▎ compiling"]
        );

        live.apply(&Event::ToolFinished {
            call: bash,
            result: Ok(String::new()),
        });
        assert!(live.calls.is_empty());
    }

    #[test]
    fn bash_keeps_the_last_lines_and_read_the_first() {
        let mut live = Live::default();
        live.apply(&Event::ToolStarted(call("1", "bash", "{}")));
        live.apply(&output("1", &numbered(1..=8)));
        live.apply(&output("1", &numbered(9..=14)));
        live.apply(&Event::ToolStarted(call("2", "read", "{}")));
        live.apply(&output("2", &numbered(1..=14)));

        assert_eq!(
            live.calls[0].body,
            numbered(5..=14).lines().collect::<Vec<_>>()
        );
        assert_eq!(
            live.calls[1].body,
            numbered(1..=10).lines().collect::<Vec<_>>()
        );
    }

    #[test]
    fn write_shows_the_start_of_its_content() {
        let mut live = Live::default();
        let content = numbered(1..=12).replace('\n', "\\n");
        let arguments = format!(r#"{{"filePath":"a.rs","content":"{content}"}}"#);
        live.apply(&Event::ToolStarted(call("1", "write", &arguments)));

        assert_eq!(
            live.calls[0].body,
            numbered(1..=10).lines().collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_oldest_calls_shrink_to_their_header_first() {
        let mut live = Live::default();
        for id in ["1", "2"] {
            live.apply(&Event::ToolStarted(call(id, "bash", "{}")));
            live.apply(&output(id, &numbered(1..=3)));
        }

        let rows = text(&live.lines(Path::new("/"), 6));
        assert_eq!(
            rows,
            ["", "▎ ▸ bash   ", "▎ ▸ bash   ", "▎ 1", "▎ 2", "▎ 3"]
        );

        let rows = text(&live.lines(Path::new("/"), 3));
        assert_eq!(rows, ["", "▎ ▸ bash   ", "▎ ▸ bash   "]);
        assert_eq!(live.lines(Path::new("/"), 2).len(), 2, "newest header only");
    }
}
