//! Prints session events for a headless run: the answer on stdout, reasoning
//! and one line per tool call on stderr, so piping stdout keeps just the answer.

use std::{io::Write, path::PathBuf};

use nth_protocol::{Event, ToolCall};
use owo_colors::OwoColorize;

pub struct Printer {
    pub tool_calls: usize,
    cwd: PathBuf,
    /// Which stream the cursor is mid-line on, so the next write from the
    /// other stream starts on a fresh line.
    mid_line: Option<Stream>,
}

#[derive(Clone, Copy, PartialEq)]
enum Stream {
    Text,
    Reasoning,
}

impl Printer {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            tool_calls: 0,
            cwd,
            mid_line: None,
        }
    }

    pub fn event(&mut self, event: &Event) {
        match event {
            Event::TextDelta(text) => self.write(Stream::Text, text),
            Event::ReasoningDelta(text) => self.write(Stream::Reasoning, text),
            Event::ToolStarted(call) => {
                self.tool_calls += 1;
                self.break_line();
                eprintln!(
                    "{} {}  {}",
                    "▸".dimmed(),
                    call.name.cyan(),
                    self.summary(call)
                );
            }
            Event::ToolFinished {
                call,
                result: Err(e),
            } => {
                self.break_line();
                let first = e.lines().next().unwrap_or_default();
                eprintln!("  {} {} {}", "✗".red(), call.name.red(), first.red());
            }
            Event::ToolFinished { .. } => {}
        }
    }

    pub fn finish(&self) {
        if self.mid_line.is_some() {
            println!();
        }
    }

    fn write(&mut self, stream: Stream, text: &str) {
        if self.mid_line.is_some_and(|s| s != stream) {
            self.break_line();
        }
        match stream {
            Stream::Text => {
                print!("{text}");
                let _ = std::io::stdout().flush();
            }
            Stream::Reasoning => {
                eprint!("{}", text.dimmed().italic());
                let _ = std::io::stderr().flush();
            }
        }
        self.mid_line = if text.ends_with('\n') {
            None
        } else {
            Some(stream)
        };
    }

    fn break_line(&mut self) {
        match self.mid_line.take() {
            Some(Stream::Text) => println!(),
            Some(Stream::Reasoning) => eprintln!(),
            None => {}
        }
    }

    /// The one argument that best says what a call is doing, with paths
    /// shown relative to the working directory.
    fn summary(&self, call: &ToolCall) -> String {
        let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments) else {
            return call.arguments.clone();
        };
        let text = ["filePath", "description", "command"]
            .iter()
            .find_map(|key| args[key].as_str())
            .unwrap_or_default();
        let cwd = self.cwd.to_string_lossy();
        match text.strip_prefix(cwd.as_ref()) {
            Some("") => ".".to_string(),
            Some(rest) => rest.trim_start_matches('/').to_string(),
            None => text.to_string(),
        }
    }
}
