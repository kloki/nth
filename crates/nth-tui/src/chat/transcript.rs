//! The chat history as a list of entries, built from session events.

mod replay;

use std::{
    ops::Range,
    path::PathBuf,
    time::{Duration, Instant},
};

use nth_protocol::{Event, NoticeSummary, ToolCall, split_notices, tool_names::writes_files};
use nth_session::plan::edits::{self, PlanEdits};
use ratatui::text::Line;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::after_write::{self, Note};
use crate::{rich::Link, settings::ChatSettings};

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    /// Where a compaction landed: the divider after what it covers. The
    /// whole summary is kept; the chat draws its first line, and the model
    /// and the clipboard take it whole.
    Compacted(String),
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

/// The run of non-blanks under display `column` of `text`, with the
/// display column it starts at; a click in a gap is on nothing.
fn word_at(text: &str, column: usize) -> Option<(usize, &str)> {
    let mut at = 0;
    // Byte and display column where the word being read started.
    let mut word: Option<(usize, usize)> = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some((start, from)) = word.take()
                && (from..at).contains(&column)
            {
                return Some((from, &text[start..i]));
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
        Some((start, from)) if (from..at).contains(&column) => Some((from, &text[start..])),
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
            Entry::Compacted(summary) => Some(summary.clone()),
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
    /// markdown link's, or a bare `http(s)://` word's, with the display
    /// columns it is drawn over. Nothing else opens, as the model writes the
    /// targets.
    pub fn link_at(&self, line: usize, column: usize) -> Option<(Range<usize>, String)> {
        let (item, row) = self.locate(line)?;
        let wrapped = item.lines.as_ref()?;
        let linked = wrapped
            .links
            .iter()
            .find(|link| link.row == row && link.columns.contains(&column))
            .map(|link| (link.columns.clone(), link.url.clone()));
        let (columns, url) = linked.or_else(|| {
            let text = wrapped.lines.get(row)?.to_string();
            word_at(&text, column).map(|(from, word)| {
                let url = word
                    .trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']', '>', '"', '\''])
                    .to_string();
                (from..from + url.width(), url)
            })
        })?;
        (url.starts_with("http://") || url.starts_with("https://")).then_some((columns, url))
    }

    /// The lines of the last layout that the entry on `line` takes, its
    /// blank separator included.
    pub fn entry_lines(&self, line: usize) -> Option<Range<usize>> {
        self.item_at(line).map(|(_, lines)| lines)
    }

    /// The item holding `line`, and which of its rows that is.
    fn locate(&self, line: usize) -> Option<(&Item, usize)> {
        self.item_at(line)
            .map(|(item, lines)| (item, line - lines.start))
    }

    /// The item holding `line`, and the lines it takes.
    fn item_at(&self, line: usize) -> Option<(&Item, Range<usize>)> {
        let mut first = 0;
        for item in &self.items {
            let rows = item.lines.as_ref().map_or(0, |w| w.lines.len());
            if line < first + rows {
                return Some((item, first..first + rows));
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
            // The tool's own row says where it went.
            Event::Usage(_) | Event::Moved(_) | Event::ContextUpdated(_) => {}
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
            Event::Compacted { summary } => {
                self.close_reasoning();
                self.push(Entry::Compacted(summary.clone()));
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
pub(super) mod tests;
