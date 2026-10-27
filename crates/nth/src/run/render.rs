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
                // The first line with something on it, as the TUI's divider
                // takes it: a model may open with blanks.
                let first = summary
                    .lines()
                    .map(str::trim)
                    .find(|line| !line.is_empty())
                    .unwrap_or_default();
                let said = match first.is_empty() {
                    true => "compacted".to_string(),
                    false => format!("compacted · {first}"),
                };
                eprintln!("{}", format!("{} {said}", icons().context).dimmed());
            }
            // The headless run prints the result's summary, not the stream.
            Event::ToolFinished { .. }
            | Event::ToolOutput { .. }
            | Event::Usage(_)
            | Event::Notice(_) => {}
        }
    }

    pub fn finish(&self) {
        match self.mid_line {
            Some(Stream::Text) => println!(),
            Some(Stream::Reasoning) => eprintln!(),
            None => {}
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
    use std::time::Duration;

    use nth_protocol::{Event, ToolCall};

    use super::*;

    /// Set only in the child the test below re-executes itself as.
    const CHILD: &str = "NTH_PRINTER_TEST_CHILD";

    fn unstyle(text: &str) -> String {
        let mut plain = String::new();
        let mut chars = text.chars();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                for code in chars.by_ref() {
                    if code == 'm' {
                        break;
                    }
                }
            } else {
                plain.push(ch);
            }
        }
        plain
    }

    #[test]
    fn tool_and_retry_events_break_lines_and_track_the_working_directory() {
        const CHILD_EVENTS: &str = "NTH_PRINTER_EVENTS_CHILD";
        if std::env::var_os(CHILD_EVENTS).is_some() {
            let mut printer = Printer::new(PathBuf::from("/repo"));
            let call = ToolCall {
                id: "one".into(),
                name: "read".into(),
                arguments: r#"{"filePath":"/other/file"}"#.into(),
            };
            printer.event(&Event::TextDelta("answer".into()));
            printer.event(&Event::Moved(PathBuf::from("/other")));
            printer.event(&Event::ToolStarted(call.clone()));
            printer.event(&Event::ToolOutput {
                call_id: call.id.clone(),
                text: "hidden output".into(),
            });
            printer.event(&Event::ToolFinished {
                call: call.clone(),
                result: Ok("hidden result".into()),
            });
            printer.event(&Event::ReasoningDelta("thinking".into()));
            printer.event(&Event::ToolFinished {
                call,
                result: Err("failed first\nfailed second".into()),
            });
            printer.event(&Event::TextDelta("retrying".into()));
            printer.event(&Event::Retry {
                attempt: 2,
                delay: Duration::from_millis(1500),
            });
            printer.event(&Event::ReasoningDelta("last thought".into()));
            printer.finish();
            assert_eq!(printer.tool_calls, 1);
            return;
        }
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "run::render::tests::tool_and_retry_events_break_lines_and_track_the_working_directory", "--nocapture"])
            .env(CHILD_EVENTS, "1")
            .env("NO_COLOR", "1")
            .output().unwrap();
        assert!(child.status.success(), "{child:?}");
        let stdout = String::from_utf8_lossy(&child.stdout);
        let stderr = unstyle(&String::from_utf8_lossy(&child.stderr));
        assert!(stdout.contains("answer\nretrying\n"), "{stdout:?}");
        assert!(stderr.contains("read  file\n"), "{stderr:?}");
        assert!(stderr.contains("thinking\n"), "{stderr:?}");
        assert!(
            stderr.contains("failed first") && !stderr.contains("failed second"),
            "{stderr:?}"
        );
        assert!(stderr.contains("retrying in 2s · attempt 2"), "{stderr:?}");
        assert!(stderr.ends_with("last thought\n"), "{stderr:?}");
        assert!(
            !stderr.contains("hidden output") && !stderr.contains("hidden result"),
            "{stderr:?}"
        );
    }

    #[test]
    fn stream_transitions_and_finish_end_the_right_lines() {
        const CHILD_TRANSITIONS: &str = "NTH_PRINTER_TRANSITIONS_CHILD";
        if std::env::var_os(CHILD_TRANSITIONS).is_some() {
            let mut printer = Printer::new(PathBuf::from("/repo"));
            printer.event(&Event::TextDelta("answer".into()));
            printer.event(&Event::TextDelta(" continued".into()));
            printer.event(&Event::ReasoningDelta("thinking".into()));
            printer.event(&Event::ReasoningDelta(" more\n".into()));
            printer.event(&Event::TextDelta("next\n".into()));
            printer.event(&Event::ReasoningDelta("unfinished".into()));
            printer.event(&Event::Moved(PathBuf::from("/other")));
            printer.event(&Event::TextDelta("last".into()));
            printer.finish();
            return;
        }
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "run::render::tests::stream_transitions_and_finish_end_the_right_lines",
                "--nocapture",
            ])
            .env(CHILD_TRANSITIONS, "1")
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert!(child.status.success(), "{child:?}");
        let stdout = String::from_utf8_lossy(&child.stdout);
        let stderr = unstyle(&String::from_utf8_lossy(&child.stderr));
        assert!(
            stdout.contains("answer continued\nnext\nlast\n"),
            "{stdout:?}"
        );
        assert!(stderr.contains("thinking more\nunfinished\n"), "{stderr:?}");
        assert!(stderr.contains("/other\n"), "{stderr:?}");
    }

    /// A compaction shows on stderr as one row holding the summary's first
    /// line with something on it, on a line of its own when the answer was
    /// mid-line; a summary that says nothing shows the word alone. Run in a
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
            // The model may open with blanks, as the TUI's divider knows.
            printer.event(&Event::Compacted {
                summary: "\n\n## Findings\n\n- one".into(),
            });
            // And a summary with nothing to show gets no dangling `·`.
            printer.event(&Event::Compacted {
                summary: "\n \n".into(),
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
            stderr.contains("compacted · ## Findings"),
            "a blank-led summary shows its first non-blank line: {stderr:?}"
        );
        assert!(
            !stderr.contains("## Details") && !stderr.contains("rest") && !stderr.contains("- one"),
            "only the summary's first line: {stderr:?}"
        );
        // One row per compaction, and a summary with nothing to show says
        // only `compacted`, without a dangling `·`.
        let rows: Vec<&str> = stderr
            .lines()
            .filter(|row| row.contains("compacted"))
            .collect();
        assert_eq!(rows.len(), 3, "{stderr:?}");
        assert!(
            !rows[2].contains('·'),
            "an empty summary gets no dangling `·`: {:?}",
            rows[2]
        );
        assert!(
            stdout.contains("partial \n"),
            "the answer's line ends before the divider: {stdout:?}"
        );
    }
}
