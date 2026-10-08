//! The chat history as a list of entries, built from session events.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use nth_protocol::{Event, Message, NoticeSummary, ToolCall, split_notices};
use nth_session::{
    SHELL_PROMPT,
    plan::edits::{self, PlanEdits},
};
use ratatui::text::Line;
use unicode_width::UnicodeWidthChar;

use super::after_write::{self, Note};
use crate::{rich::Link, settings::ChatSettings};

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    /// What a background monitor said, as the model got it.
    Notice(NoticeSummary),
    /// The plan as you edited it with ctrl+g.
    PlanEdits(PlanEdits),
    /// The model's thinking, shown in full under a line timing it.
    Reasoning {
        started: Instant,
        took: Option<Duration>,
        text: String,
    },
    Answer(String),
    /// A provider error being retried, shown as its own row; the retry's
    /// text follows in a fresh answer block.
    Retry {
        attempt: u32,
        delay: Duration,
    },
    Tool {
        call: ToolCall,
        state: ToolState,
        /// What the call produced, at most `OUTPUT_LINES`, kept after it
        /// finishes so it can be read back.
        output: Vec<String>,
        /// What a write reported once done: formatters run, errors found.
        notes: Vec<Note>,
    },
    TurnError(String),
    TurnDone {
        model: String,
        tool_calls: usize,
        elapsed: Duration,
    },
    /// Footer of a turn the user stopped with Esc.
    Interrupted {
        elapsed: Duration,
    },
}

/// The run of non-blanks under display `column` of `text`; a click in a
/// gap is on nothing.
fn word_at(text: &str, column: usize) -> Option<&str> {
    let mut at = 0;
    // Byte and display column where the word being read started.
    let mut word: Option<(usize, usize)> = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some((start, from)) = word.take()
                && (from..at).contains(&column)
            {
                return Some(&text[start..i]);
            }
            if column < at {
                return None;
            }
        } else if word.is_none() {
            word = Some((i, at));
        }
        at += c.width().unwrap_or(0);
    }
    match word {
        Some((start, from)) if (from..at).contains(&column) => Some(&text[start..]),
        _ => None,
    }
}

/// The most output a tool row keeps.
pub(super) const OUTPUT_LINES: usize = 10;
/// Where the session appends a reminder about the mode to what you typed.
const REMINDER: &str = "\n\n<system-reminder>\n";

#[derive(Debug, Clone, PartialEq)]
pub enum ToolState {
    Running,
    Done,
    /// First line of the error, which is all a one-line row has room for.
    Failed(String),
}

pub struct Transcript {
    pub(super) cwd: PathBuf,
    pub(super) items: Vec<Item>,
    /// Width the cached lines were wrapped for.
    pub(super) width: u16,
    /// What the cached lines show.
    pub(super) settings: ChatSettings,
}

pub(super) struct Item {
    pub(super) entry: Entry,
    /// How the entry wraps, including the blank line that separates it
    /// from the previous one. Dropped whenever the entry or width changes.
    pub(super) lines: Option<Wrapped>,
}

/// An entry's rows for one width, and the links on them.
pub(super) struct Wrapped {
    pub(super) lines: Vec<Line<'static>>,
    /// `row` counts from the entry's first line, the blank one included.
    pub(super) links: Vec<Link>,
}

impl Entry {
    /// What a right click on the entry copies: what it says, as written,
    /// so an answer comes out as markdown. Rows that only report on the
    /// turn copy nothing.
    pub fn clipboard(&self) -> Option<String> {
        match self {
            Entry::User(text) | Entry::Answer(text) | Entry::TurnError(text) => Some(text.clone()),
            Entry::Reasoning { text, .. } => Some(text.clone()),
            Entry::Tool { output, .. } => Some(output.join("\n")),
            Entry::Notice(_)
            | Entry::PlanEdits(_)
            | Entry::Retry { .. }
            | Entry::TurnDone { .. }
            | Entry::Interrupted { .. } => None,
        }
    }
}

impl Transcript {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            items: Vec::new(),
            width: 0,
            settings: ChatSettings::default(),
        }
    }

    /// The history of a resumed session, rebuilt by replaying its messages
    /// as the events they once streamed as. Timings are not saved, so turns
    /// get no footers and reasoning shows no duration.
    pub fn replay(cwd: PathBuf, messages: &[Message]) -> Self {
        let mut t = Self::new(cwd);
        for message in messages {
            match message {
                Message::System(_) => {}
                // A command you ran shows as its bash row alone, as it did live.
                Message::User(text) if text == SHELL_PROMPT => {}
                // A skill run as `/name args` shows as typed, as it did live.
                Message::User(text) => match text.split_once("\n\n<skill_content ") {
                    Some((command, _)) => t.push_user(command.to_string()),
                    None => t.push_user(text.clone()),
                },
                Message::Assistant(reply) => {
                    if !reply.reasoning.is_empty() {
                        t.push(Entry::Reasoning {
                            started: Instant::now(),
                            took: Some(Duration::ZERO),
                            text: reply.reasoning.clone(),
                        });
                    }
                    t.apply(&Event::TextDelta(reply.text.clone()));
                    for call in &reply.tool_calls {
                        t.apply(&Event::ToolStarted(call.clone()));
                    }
                }
                Message::ToolResult { call_id, content } => {
                    let Some(Item {
                        entry: Entry::Tool { call, .. },
                        ..
                    }) = t.tool_mut(call_id)
                    else {
                        continue;
                    };
                    let call = call.clone();
                    // The agent loop saves a failed call's error this way.
                    let result = match content.strip_prefix("Error: ") {
                        Some(error) => Err(error.to_string()),
                        // A write streamed nothing; its start showed its content.
                        None if call.name == "write" => Ok(content.clone()),
                        // A skill is one row; its body is for the model.
                        None if call.name == "skill" => Ok(content.clone()),
                        // A question is one row too; the answers are for the model.
                        None if call.name == "question" => Ok(content.clone()),
                        // The panel it switched to says what it did.
                        None if call.name == "panel" => Ok(content.clone()),
                        // Its page shows once finished, as it did live.
                        None if call.name == "webfetch" => Ok(content.clone()),
                        // An edit shows its diff, from its arguments; what
                        // it reported after writing shows as notes.
                        None if call.name == "edit" => Ok(content.clone()),
                        None => {
                            // read appends instruction files for the model
                            // only; the live view never showed them. The
                            // notes after an edit show as notes.
                            let shown = match content.split_once("\n\n<system-reminder>") {
                                Some((shown, _)) if call.name == "read" => shown,
                                _ if writes_files(&call.name) => content
                                    .split_once("\n\n")
                                    .map_or(content.as_str(), |(s, _)| s),
                                _ => content,
                            };
                            t.apply(&Event::ToolOutput {
                                call_id: call_id.clone(),
                                text: shown.to_string(),
                            });
                            Ok(content.clone())
                        }
                    };
                    t.apply(&Event::ToolFinished { call, result });
                }
            }
        }
        t
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &Entry> {
        self.items.iter().map(|item| &item.entry)
    }

    /// The entry on `line` of the last layout.
    pub fn entry_at(&self, line: usize) -> Option<&Entry> {
        self.locate(line).map(|(item, _)| &item.entry)
    }

    /// The web address on `line` at display `column` of the last layout: a
    /// markdown link's, or a bare `http(s)://` word's. Nothing else opens,
    /// as the model writes the targets.
    pub fn link_at(&self, line: usize, column: usize) -> Option<String> {
        let (item, row) = self.locate(line)?;
        let wrapped = item.lines.as_ref()?;
        let linked = wrapped
            .links
            .iter()
            .find(|link| link.row == row && link.columns.contains(&column))
            .map(|link| link.url.clone());
        let url = linked.or_else(|| {
            let text = wrapped.lines.get(row)?.to_string();
            word_at(&text, column).map(|word| {
                word.trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '>', '"', '\''])
                    .to_string()
            })
        })?;
        (url.starts_with("http://") || url.starts_with("https://")).then_some(url)
    }

    /// The item holding `line`, and which of its rows that is.
    fn locate(&self, line: usize) -> Option<(&Item, usize)> {
        let mut first = 0;
        for item in &self.items {
            let rows = item.lines.as_ref().map_or(0, |w| w.lines.len());
            if line < first + rows {
                return Some((item, line - first));
            }
            first += rows;
        }
        None
    }

    /// What the model got as a user message: what monitors said, shown as
    /// one row each, then what you typed, if anything. A system reminder
    /// the session appended about the mode is for the model only.
    pub fn push_user(&mut self, text: String) {
        let text = match text.split_once(REMINDER) {
            Some((typed, _)) => typed.to_string(),
            None => text,
        };
        let (notices, typed) = split_notices(&text);
        for notice in notices {
            self.push(Entry::Notice(notice));
        }
        if typed.is_empty() {
            return;
        }
        // The diff and instruction ctrl+g sends are for the model; the
        // chat says only that the plan was edited.
        match edits::parse(typed) {
            Some(edits) => self.push(Entry::PlanEdits(edits)),
            None => self.push(Entry::User(typed.to_string())),
        }
    }

    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::ReasoningDelta(delta) => match self.items.last_mut() {
                Some(Item {
                    entry:
                        Entry::Reasoning {
                            took: None, text, ..
                        },
                    lines,
                }) => {
                    text.push_str(delta);
                    *lines = None;
                }
                _ => self.push(Entry::Reasoning {
                    started: Instant::now(),
                    took: None,
                    text: delta.clone(),
                }),
            },
            Event::TextDelta(text) => {
                self.close_reasoning();
                match self.items.last_mut() {
                    Some(
                        item @ Item {
                            entry: Entry::Answer(_),
                            ..
                        },
                    ) => {
                        if let Entry::Answer(answer) = &mut item.entry {
                            answer.push_str(text);
                        }
                        item.lines = None;
                    }
                    // A lone newline before the answer would open an empty block.
                    _ if text.trim().is_empty() => {}
                    _ => self.push(Entry::Answer(text.clone())),
                }
            }
            Event::ToolStarted(call) => {
                self.close_reasoning();
                let mut output = Vec::new();
                // A write's content is in its arguments; it streams nothing.
                if call.name == "write" {
                    let args = serde_json::from_str::<serde_json::Value>(&call.arguments);
                    if let Some(content) = args.ok().as_ref().and_then(|a| a["content"].as_str()) {
                        keep_output(&call.name, &mut output, content);
                    }
                }
                self.push(Entry::Tool {
                    call: call.clone(),
                    state: ToolState::Running,
                    output,
                    notes: Vec::new(),
                });
            }
            Event::ToolOutput { call_id, text } => {
                if let Some(Item {
                    entry: Entry::Tool { call, output, .. },
                    lines,
                }) = self.tool_mut(call_id)
                {
                    keep_output(&call.name, output, text);
                    *lines = None;
                }
            }
            Event::ToolFinished { call, result } => {
                if let Some(Item {
                    entry:
                        Entry::Tool {
                            state,
                            output,
                            notes,
                            ..
                        },
                    lines,
                }) = self.tool_mut(&call.id)
                {
                    if let Ok(text) = result
                        && writes_files(&call.name)
                    {
                        *notes = after_write::parse(text);
                    }
                    // A fetch streams nothing; its page is its result.
                    if let Ok(text) = result
                        && call.name == "webfetch"
                    {
                        keep_output(&call.name, output, text);
                    }
                    *state = match result {
                        Ok(_) => ToolState::Done,
                        Err(e) => ToolState::Failed(e.lines().next().unwrap_or_default().into()),
                    };
                    *lines = None;
                }
            }
            Event::Usage(_) => {}
            Event::Retry { attempt, delay } => {
                self.close_reasoning();
                self.push(Entry::Retry {
                    attempt: *attempt,
                    delay: *delay,
                });
            }
            Event::Notice(text) => {
                self.close_reasoning();
                self.push_user(text.clone());
            }
        }
    }

    /// A problem outside the turn itself, such as the session not saving.
    pub fn push_error(&mut self, error: String) {
        self.push(Entry::TurnError(error));
    }

    /// Closes the turn with its footer, or with the error that ended it.
    pub fn finish_turn(&mut self, result: Result<(), String>, model: &str, elapsed: Duration) {
        self.close_reasoning();
        let entry = match result {
            Ok(()) => Entry::TurnDone {
                model: model.to_string(),
                tool_calls: self.tool_calls_this_turn(),
                elapsed,
            },
            Err(e) => Entry::TurnError(e),
        };
        self.push(entry);
    }

    /// Closes a turn the user cut short. Tools still running never report
    /// back, so they are marked as stopped here.
    pub fn interrupt(&mut self, elapsed: Duration) {
        self.close_reasoning();
        self.stop_running_tools("interrupted");
        self.push(Entry::Interrupted { elapsed });
    }

    /// Closes a turn whose task died under it, with `error` as the footer;
    /// its tools never report back either.
    pub fn fail_turn(&mut self, error: String) {
        self.close_reasoning();
        self.stop_running_tools("lost");
        self.push(Entry::TurnError(error));
    }

    fn stop_running_tools(&mut self, why: &str) {
        for item in &mut self.items {
            if let Entry::Tool { state, .. } = &mut item.entry
                && *state == ToolState::Running
            {
                *state = ToolState::Failed(why.into());
                item.lines = None;
            }
        }
    }

    fn push(&mut self, entry: Entry) {
        self.items.push(Item { entry, lines: None });
    }

    fn tool_mut(&mut self, id: &str) -> Option<&mut Item> {
        self.items
            .iter_mut()
            .rev()
            .find(|item| matches!(&item.entry, Entry::Tool { call, .. } if call.id == id))
    }

    fn close_reasoning(&mut self) {
        if let Some(item) = self.items.last_mut()
            && let Entry::Reasoning { started, took, .. } = &mut item.entry
            && took.is_none()
        {
            *took = Some(started.elapsed());
            item.lines = None;
        }
    }

    /// The tool calls of the turn being closed: back to whatever started
    /// it, or ended the one before. A turn that only monitors' notices
    /// started has no user row, so stopping at `User` alone would count
    /// the previous turn's calls again.
    fn tool_calls_this_turn(&self) -> usize {
        self.entries()
            .rev()
            .take_while(|entry| {
                !matches!(
                    entry,
                    Entry::User(_)
                        | Entry::Notice(_)
                        | Entry::PlanEdits(_)
                        | Entry::TurnDone { .. }
                        | Entry::TurnError(_)
                        | Entry::Interrupted { .. }
                )
            })
            .filter(|entry| matches!(entry, Entry::Tool { .. }))
            .count()
    }
}

/// The tools whose results end with format notes and LSP errors.
fn writes_files(tool: &str) -> bool {
    matches!(tool, "write" | "edit" | "apply_patch")
}

/// Adds `text` to a call's output, keeping the lines worth seeing: the end
/// of a command's output, where it has got to, but the top of a file, where
/// it says what it is.
fn keep_output(tool: &str, output: &mut Vec<String>, text: &str) {
    let lines = text.lines().map(|line| line.replace('\t', "    "));
    if tool == "bash" {
        output.extend(lines);
        let extra = output.len().saturating_sub(OUTPUT_LINES);
        output.drain(..extra);
    } else {
        let room = OUTPUT_LINES.saturating_sub(output.len());
        output.extend(lines.take(room));
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::chat) fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: r#"{"filePath":"/repo/src/a.rs"}"#.into(),
        }
    }

    pub(in crate::chat) fn transcript() -> Transcript {
        Transcript::new("/repo".into())
    }

    pub(in crate::chat) fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    #[test]
    fn a_word_is_only_under_its_own_columns() {
        assert_eq!(word_at("a  bb c", 0), Some("a"));
        assert_eq!(word_at("a  bb c", 1), None);
        assert_eq!(word_at("a  bb c", 2), None);
        assert_eq!(word_at("a  bb c", 3), Some("bb"));
        assert_eq!(word_at("a  bb c", 6), Some("c"));
        assert_eq!(word_at("a  bb c", 7), None);
        assert_eq!(word_at("日本 x", 1), Some("日本"));
    }

    #[test]
    fn entries_are_found_by_line_with_their_blank_separators() {
        let mut t = transcript();
        t.push_user("one".into());
        t.apply(&Event::TextDelta("two".into()));
        t.push_error("three".into());
        t.layout(40, ChatSettings::default());

        assert_eq!(t.entry_at(0), Some(&Entry::User("one".into())));
        // The blank line above an entry counts as its own.
        assert_eq!(t.entry_at(1), Some(&Entry::Answer("two".into())));
        assert_eq!(t.entry_at(2), Some(&Entry::Answer("two".into())));
        assert_eq!(t.entry_at(4), Some(&Entry::TurnError("three".into())));
        assert_eq!(t.entry_at(5), None);
    }

    #[test]
    fn links_are_found_by_their_drawn_columns() {
        let mut t = transcript();
        t.apply(&Event::TextDelta(
            "see [docs](https://x.y) or https://a.b/c. and [no](file:///etc/passwd)".into(),
        ));
        t.layout(60, ChatSettings::default());
        assert_eq!(
            text(&t.visible(0, 1)),
            ["▎ see docs or https://a.b/c. and no"]
        );

        // `docs` sits after the bar and `see `.
        assert_eq!(t.link_at(0, 6).as_deref(), Some("https://x.y"));
        assert_eq!(t.link_at(0, 9).as_deref(), Some("https://x.y"));
        assert_eq!(t.link_at(0, 10), None);
        // A bare address, without the full stop after it.
        assert_eq!(t.link_at(0, 14).as_deref(), Some("https://a.b/c"));
        // The gap before it opens nothing, nor does a file link.
        assert_eq!(t.link_at(0, 13), None);
        assert_eq!(t.link_at(0, 35), None);
        assert_eq!(t.link_at(1, 6), None);
    }

    #[test]
    fn a_link_on_a_wrapped_row_is_found_there() {
        let mut t = transcript();
        t.apply(&Event::TextDelta(
            "alpha beta gamma [delta](https://d) epsilon".into(),
        ));
        t.layout(20, ChatSettings::default());
        assert_eq!(
            text(&t.visible(0, 3)),
            ["▎ alpha beta gamma", "▎ delta epsilon"]
        );

        assert_eq!(t.link_at(1, 2).as_deref(), Some("https://d"));
        assert_eq!(t.link_at(1, 6).as_deref(), Some("https://d"));
        assert_eq!(t.link_at(1, 7), None);
        assert_eq!(t.link_at(0, 2), None);
    }

    #[test]
    fn monitor_notices_show_as_their_own_rows() {
        let text = "<monitor id=\"2\" description=\"ci\" log=\"/l/2.log\">\nfailed\n</monitor>\n\
                    <monitor id=\"2\" description=\"ci\" log=\"/l/2.log\" ended=\"exited with code 1\" events=\"1\"/>\n\nfix it";
        let t = Transcript::replay("/repo".into(), &[Message::User(text.into())]);

        let entries: Vec<_> = t.entries().collect();
        let notice = |lines, ended: Option<&str>| {
            Entry::Notice(NoticeSummary::Monitor {
                id: 2,
                description: "ci".into(),
                lines,
                ended: ended.map(Into::into),
            })
        };
        assert_eq!(
            entries,
            [
                &notice(1, None),
                &notice(0, Some("exited with code 1")),
                &Entry::User("fix it".into()),
            ]
        );
    }

    #[test]
    fn streams_reasoning_tools_and_answer_into_entries() {
        let mut t = transcript();
        t.push_user("go".into());
        t.apply(&Event::ReasoningDelta("hm".into()));
        t.apply(&Event::ReasoningDelta("mm".into()));
        t.apply(&Event::ToolStarted(call("1")));
        t.apply(&Event::ToolFinished {
            call: call("1"),
            result: Err("no such file\nat line 2".into()),
        });
        t.apply(&Event::TextDelta("\n".into()));
        t.apply(&Event::TextDelta("do".into()));
        t.apply(&Event::TextDelta("ne".into()));
        t.finish_turn(Ok(()), "glm", Duration::from_secs(2));

        let entries: Vec<_> = t.entries().collect();
        assert_eq!(entries.len(), 5);
        assert!(matches!(
            entries[1],
            Entry::Reasoning { took: Some(_), text, .. } if text == "hmmm"
        ));
        assert_eq!(
            entries[2],
            &Entry::Tool {
                call: call("1"),
                state: ToolState::Failed("no such file".into()),
                output: Vec::new(),
                notes: Vec::new(),
            }
        );
        assert_eq!(entries[3], &Entry::Answer("done".into()));
        assert!(matches!(entries[4], Entry::TurnDone { tool_calls: 1, .. }));
    }

    #[test]
    fn a_retry_row_sits_between_the_partial_and_the_retried_answer() {
        let mut t = transcript();
        t.push_user("go".into());
        t.apply(&Event::TextDelta("partial".into()));
        t.apply(&Event::Retry {
            attempt: 1,
            delay: Duration::from_secs(2),
        });
        t.apply(&Event::TextDelta("full answer".into()));

        let entries: Vec<_> = t.entries().collect();
        assert_eq!(
            entries,
            [
                &Entry::User("go".into()),
                &Entry::Answer("partial".into()),
                &Entry::Retry {
                    attempt: 1,
                    delay: Duration::from_secs(2),
                },
                &Entry::Answer("full answer".into()),
            ],
            "the retry's text starts a fresh answer block"
        );
    }

    #[test]
    fn a_turn_counts_only_its_own_tool_calls() {
        let mut t = transcript();
        t.push_user("go".into());
        t.apply(&Event::ToolStarted(call("1")));
        t.apply(&Event::ToolStarted(call("2")));
        t.finish_turn(Ok(()), "glm", Duration::from_secs(1));
        // A turn that monitors' notices started has no user row.
        t.push_user(
            "<monitor id=\"2\" description=\"ci\" log=\"/l/2.log\">\nfailed\n</monitor>".into(),
        );
        t.apply(&Event::ToolStarted(call("3")));
        t.finish_turn(Ok(()), "glm", Duration::from_secs(1));
        // One that answered without tools, after a stopped one.
        t.interrupt(Duration::from_secs(1));
        t.finish_turn(Ok(()), "glm", Duration::from_secs(1));

        let counts: Vec<_> = t
            .entries()
            .filter_map(|entry| match entry {
                Entry::TurnDone { tool_calls, .. } => Some(*tool_calls),
                _ => None,
            })
            .collect();
        assert_eq!(counts, [2, 1, 0]);
    }

    #[test]
    fn interrupting_stops_running_tools_and_adds_the_footer() {
        let mut t = transcript();
        t.push_user("go".into());
        t.apply(&Event::ReasoningDelta("hm".into()));
        t.apply(&Event::ToolStarted(call("1")));
        t.apply(&Event::ToolStarted(call("2")));
        t.apply(&Event::ToolFinished {
            call: call("1"),
            result: Ok("x".into()),
        });
        t.interrupt(Duration::from_secs(3));

        let entries: Vec<_> = t.entries().collect();
        assert!(matches!(entries[1], Entry::Reasoning { took: Some(_), .. }));
        assert!(matches!(
            entries[2],
            Entry::Tool {
                state: ToolState::Done,
                ..
            }
        ));
        assert_eq!(
            entries[3],
            &Entry::Tool {
                call: call("2"),
                state: ToolState::Failed("interrupted".into()),
                output: Vec::new(),
                notes: Vec::new(),
            }
        );
        assert!(matches!(entries[4], Entry::Interrupted { .. }));
        let total = t.layout(40, ChatSettings::default());
        assert_eq!(text(&t.visible(total - 1, 1)), ["  ⏹ interrupted · 3.0s"]);
    }

    fn tool(id: &str, name: &str, arguments: &str) -> ToolCall {
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

    fn numbered(range: std::ops::RangeInclusive<usize>) -> Vec<String> {
        range.map(|i| i.to_string()).collect()
    }

    fn outputs(t: &Transcript) -> Vec<Vec<String>> {
        t.entries()
            .filter_map(|entry| match entry {
                Entry::Tool { output, .. } => Some(output.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn output_stays_with_its_call_after_it_finishes() {
        let mut t = transcript();
        let bash = tool("1", "bash", r#"{"command":"cargo test"}"#);
        t.apply(&Event::ToolStarted(bash.clone()));
        t.apply(&output("1", "compiling\n"));
        t.apply(&output("2", "not this call\n"));
        t.apply(&Event::ToolFinished {
            call: bash,
            result: Ok(String::new()),
        });

        assert_eq!(outputs(&t), [["compiling"]]);
    }

    #[test]
    fn bash_keeps_the_last_lines_and_read_the_first() {
        let mut t = transcript();
        let text = |range| numbered(range).join("\n") + "\n";
        t.apply(&Event::ToolStarted(tool("1", "bash", "{}")));
        t.apply(&Event::ToolStarted(tool("2", "read", "{}")));
        t.apply(&output("1", &text(1..=8)));
        t.apply(&output("1", &text(9..=14)));
        t.apply(&output("2", &text(1..=14)));

        assert_eq!(outputs(&t), [numbered(5..=14), numbered(1..=10)]);
    }

    #[test]
    fn write_shows_the_start_of_its_content() {
        let mut t = transcript();
        let content = numbered(1..=12).join("\\n");
        let arguments = format!(r#"{{"filePath":"a.rs","content":"{content}"}}"#);
        t.apply(&Event::ToolStarted(tool("1", "write", &arguments)));

        assert_eq!(outputs(&t), [numbered(1..=10)]);
    }

    #[test]
    fn plan_edits_show_as_one_row() {
        let text = edits::render("/repo/plan.md".as_ref(), "a\n", "a\n<!-- b? -->\n");
        let mut t = Transcript::new("/repo".into());
        t.push_user(text);

        assert_eq!(
            t.entries().collect::<Vec<_>>(),
            [&Entry::PlanEdits(PlanEdits {
                added: 1,
                removed: 0
            })]
        );
    }

    #[test]
    fn mode_reminders_are_left_out() {
        let mut t = Transcript::new("/repo".into());
        t.push_user(
            "plan it\n\n<system-reminder>\nPlan mode is active.\n</system-reminder>".into(),
        );
        t.push_user("go\n\n<system-reminder>\nYour operational mode has changed\n</system-reminder>\n\nA plan file exists".into());

        let users: Vec<_> = t.entries().collect();
        assert_eq!(
            users,
            [&Entry::User("plan it".into()), &Entry::User("go".into())]
        );
    }

    #[test]
    fn replays_a_command_you_ran_as_its_bash_row() {
        let bash = tool("1", "bash", r#"{"command":"ls"}"#);
        let messages = [
            Message::User(SHELL_PROMPT.into()),
            Message::Assistant(nth_protocol::AssistantMessage {
                tool_calls: vec![bash],
                ..Default::default()
            }),
            Message::ToolResult {
                call_id: "1".into(),
                content: "a.rs".into(),
            },
        ];

        let t = Transcript::replay(PathBuf::from("/repo"), &messages);

        let entries: Vec<_> = t.entries().collect();
        assert!(
            matches!(&entries[..], [Entry::Tool { call, state: ToolState::Done, .. }] if call.name == "bash"),
            "{entries:?}"
        );
    }

    #[test]
    fn replays_a_saved_session() {
        let bash = tool("1", "bash", r#"{"command":"ls"}"#);
        let messages = [
            Message::System("you are nth".into()),
            Message::User("go".into()),
            Message::Assistant(nth_protocol::AssistantMessage {
                text: "looking".into(),
                reasoning: "hm".into(),
                tool_calls: vec![bash.clone(), call("2")],
            }),
            Message::ToolResult {
                call_id: "1".into(),
                content: "a.rs\nb.rs".into(),
            },
            Message::ToolResult {
                call_id: "2".into(),
                content: "Error: no such file".into(),
            },
            Message::Assistant(nth_protocol::AssistantMessage {
                text: "done".into(),
                ..Default::default()
            }),
        ];

        let t = Transcript::replay("/repo".into(), &messages);

        let entries: Vec<_> = t.entries().collect();
        assert_eq!(entries[0], &Entry::User("go".into()));
        assert!(matches!(
            entries[1],
            Entry::Reasoning { took: Some(_), text, .. } if text == "hm"
        ));
        assert_eq!(entries[2], &Entry::Answer("looking".into()));
        assert_eq!(
            entries[3],
            &Entry::Tool {
                call: bash,
                state: ToolState::Done,
                output: vec!["a.rs".into(), "b.rs".into()],
                notes: Vec::new(),
            }
        );
        assert_eq!(
            entries[4],
            &Entry::Tool {
                call: call("2"),
                state: ToolState::Failed("no such file".into()),
                output: Vec::new(),
                notes: Vec::new(),
            }
        );
        assert_eq!(entries[5], &Entry::Answer("done".into()));
        assert_eq!(entries.len(), 6);
    }

    #[test]
    fn replay_keeps_what_a_write_reported() {
        let write = tool("1", "write", r#"{"filePath":"/repo/a.rs","content":"x"}"#);
        let messages = [
            Message::User("go".into()),
            Message::Assistant(nth_protocol::AssistantMessage {
                tool_calls: vec![write],
                ..Default::default()
            }),
            Message::ToolResult {
                call_id: "1".into(),
                content: "Wrote file: /repo/a.rs\n\nFormatted with rustfmt.".into(),
            },
        ];

        let t = Transcript::replay("/repo".into(), &messages);

        let Some(Entry::Tool { output, notes, .. }) = t.entries().nth(1) else {
            panic!("no write entry");
        };
        assert_eq!(output, &["x"]);
        assert_eq!(notes, &[Note::Format("Formatted with rustfmt.".into())]);
    }

    #[test]
    fn replay_keeps_what_an_edit_reported_as_notes() {
        let edit = tool("1", "edit", r#"{"filePath":"/repo/a.rs"}"#);
        let messages = [
            Message::User("go".into()),
            Message::Assistant(nth_protocol::AssistantMessage {
                tool_calls: vec![edit],
                ..Default::default()
            }),
            Message::ToolResult {
                call_id: "1".into(),
                content: "Edited file: /repo/a.rs\n\nFormatted with rustfmt.".into(),
            },
        ];

        let t = Transcript::replay("/repo".into(), &messages);

        let Some(Entry::Tool { output, notes, .. }) = t.entries().nth(1) else {
            panic!("no edit entry");
        };
        assert!(output.is_empty(), "its diff comes from its arguments");
        assert_eq!(notes, &[Note::Format("Formatted with rustfmt.".into())]);
    }

    #[test]
    fn replay_shows_a_skill_command_as_typed() {
        let messages = [Message::User(
            "/fix the build\n\n<skill_content name=\"fix\">\n# Skill: fix\n</skill_content>".into(),
        )];

        let t = Transcript::replay("/repo".into(), &messages);

        assert_eq!(
            t.entries().collect::<Vec<_>>(),
            [&Entry::User("/fix the build".into())]
        );
    }

    #[test]
    fn replay_shows_a_skill_without_its_body() {
        let skill = tool("1", "skill", r#"{"name":"deploy"}"#);
        let messages = [
            Message::User("go".into()),
            Message::Assistant(nth_protocol::AssistantMessage {
                tool_calls: vec![skill.clone()],
                ..Default::default()
            }),
            Message::ToolResult {
                call_id: "1".into(),
                content: "<skill_content name=\"deploy\">\n# Skill: deploy\n</skill_content>"
                    .into(),
            },
        ];

        let t = Transcript::replay("/repo".into(), &messages);

        assert_eq!(
            t.entries().nth(1),
            Some(&Entry::Tool {
                call: skill,
                state: ToolState::Done,
                output: Vec::new(),
                notes: Vec::new(),
            })
        );
    }

    #[test]
    fn replay_hides_instructions_attached_to_a_read() {
        let messages = [
            Message::User("go".into()),
            Message::Assistant(nth_protocol::AssistantMessage {
                tool_calls: vec![call("1")],
                ..Default::default()
            }),
            Message::ToolResult {
                call_id: "1".into(),
                content: "1: fn main() {}\n\n<system-reminder>\nInstructions from: /repo/src/AGENTS.md\nBe brief.\n</system-reminder>\n".into(),
            },
        ];

        let t = Transcript::replay("/repo".into(), &messages);

        assert_eq!(outputs(&t), [vec!["1: fn main() {}".to_string()]]);
    }
}
