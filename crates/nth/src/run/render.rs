//! Prints session events for a headless run: the answer on stdout, reasoning
//! and one line per tool call on stderr, so piping stdout keeps just the answer.

use std::{io::Write, path::PathBuf};

use nth_icons::icons;
use nth_protocol::{Event, retry_label};
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
                    icons().current.dimmed(),
                    call.name.cyan(),
                    call.summary(&self.cwd)
                );
            }
            Event::ToolFinished {
                call,
                result: Err(e),
            } => {
                self.break_line();
                let first = e.lines().next().unwrap_or_default();
                eprintln!(
                    "  {} {} {}",
                    icons().fail.red(),
                    call.name.red(),
                    first.red()
                );
            }
            Event::Retry { attempt, delay } => {
                self.break_line();
                eprintln!(
                    "{}",
                    format!("{} {}", icons().retry, retry_label(*attempt, *delay)).yellow()
                );
            }
            // Later calls are summarised relative to where they now run.
            Event::Moved(cwd) => {
                self.break_line();
                eprintln!("{}", format!("{} {}", icons().to, cwd.display()).dimmed());
                self.cwd = cwd.clone();
            }
            Event::Compacted { summary } => {
                self.break_line();
                let first = summary.lines().next().unwrap_or_default().trim();
                eprintln!(
                    "{}",
                    format!("{} compacted · {first}", icons().context).dimmed()
                );
            }
            // The headless run prints the result's summary, not the stream.
            Event::ToolFinished { .. }
            | Event::ToolOutput { .. }
            | Event::Usage(_)
            | Event::Notice(_) => {}
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
}

#[cfg(test)]
mod tests {
    use nth_protocol::Event;

    use super::*;

    /// Set only in the child the test below re-executes itself as.
    const CHILD: &str = "NTH_PRINTER_TEST_CHILD";

    /// A compaction shows on stderr as one row holding the summary's first
    /// line, on a line of its own when the answer was mid-line. Run in a
    /// child process, since the printer writes to the real streams.
    #[test]
    fn a_compaction_prints_one_row_with_the_first_summary_line() {
        if std::env::var_os(CHILD).is_some() {
            // The child: print as the headless run's task does.
            let mut printer = Printer::new(PathBuf::from("/repo"));
            printer.event(&Event::TextDelta("partial ".into()));
            printer.event(&Event::Compacted {
                summary: "Objective: ship\n\n## Details\nrest".into(),
            });
            printer.finish();
            return;
        }

        let child = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "run::render::tests::a_compaction_prints_one_row_with_the_first_summary_line",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .expect("runs the child");

        assert!(child.status.success(), "{child:?}");
        let stdout = String::from_utf8_lossy(&child.stdout);
        let stderr = String::from_utf8_lossy(&child.stderr);
        assert!(
            stderr.contains("compacted · Objective: ship"),
            "the divider's row on stderr: {stderr:?}"
        );
        assert!(
            !stderr.contains("## Details") && !stderr.contains("rest"),
            "only the summary's first line: {stderr:?}"
        );
        assert!(
            stdout.contains("partial \n"),
            "the answer's line ends before the divider: {stdout:?}"
        );
    }
}
