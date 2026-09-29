//! The chat history as the TUI shows it, built from session events and
//! wrapped into styled lines for the current width.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use nth_protocol::{Event, ToolCall};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

const BAR: &str = "▎ ";
const BAR_WIDTH: u16 = 2;
/// Compact lines are indented to sit under the text of a barred block.
const INDENT: &str = "  ";

#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    User(String),
    /// Shown as one line; the reasoning text itself stays in the session,
    /// where the model needs it.
    Reasoning {
        started: Instant,
        took: Option<Duration>,
    },
    Answer(String),
    Tool {
        call: ToolCall,
        state: ToolState,
    },
    TurnError(String),
    TurnDone {
        model: String,
        tool_calls: usize,
        elapsed: Duration,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolState {
    Running,
    Done,
    /// First line of the error, which is all a one-line row has room for.
    Failed(String),
}

/// What the running turn is doing right now, for the status row.
#[derive(Debug, Clone, PartialEq)]
pub enum Activity<'a> {
    Thinking,
    Writing,
    Tool(&'a ToolCall),
}

pub struct Transcript {
    cwd: PathBuf,
    items: Vec<Item>,
    width: u16,
}

struct Item {
    entry: Entry,
    /// Wrapped lines, including the blank line that separates this entry
    /// from the previous one. Dropped whenever the entry or width changes.
    lines: Option<Vec<Line<'static>>>,
}

impl Transcript {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            items: Vec::new(),
            width: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &Entry> {
        self.items.iter().map(|item| &item.entry)
    }

    pub fn push_user(&mut self, text: String) {
        self.push(Entry::User(text));
    }

    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::ReasoningDelta(_) => {
                if !matches!(self.last(), Some(Entry::Reasoning { took: None, .. })) {
                    self.push(Entry::Reasoning {
                        started: Instant::now(),
                        took: None,
                    });
                }
            }
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
                self.push(Entry::Tool {
                    call: call.clone(),
                    state: ToolState::Running,
                });
            }
            Event::ToolFinished { call, result } => {
                let item = self.items.iter_mut().rev().find(
                    |item| matches!(&item.entry, Entry::Tool { call: c, .. } if c.id == call.id),
                );
                if let Some(Item {
                    entry: Entry::Tool { state, .. },
                    lines,
                }) = item
                {
                    *state = match result {
                        Ok(_) => ToolState::Done,
                        Err(e) => ToolState::Failed(e.lines().next().unwrap_or_default().into()),
                    };
                    *lines = None;
                }
            }
        }
    }

    /// Closes the turn with its footer, or with the error that ended it.
    pub fn finish_turn(&mut self, result: Result<(), String>, model: &str, elapsed: Duration) {
        self.close_reasoning();
        let entry = match result {
            Ok(()) => Entry::TurnDone {
                model: model.to_string(),
                tool_calls: self.tool_calls_since_user(),
                elapsed,
            },
            Err(e) => Entry::TurnError(e),
        };
        self.push(entry);
    }

    pub fn activity(&self) -> Activity<'_> {
        let running = self.entries().rev().find_map(|entry| match entry {
            Entry::Tool {
                call,
                state: ToolState::Running,
            } => Some(call),
            _ => None,
        });
        match (running, self.last()) {
            (Some(call), _) => Activity::Tool(call),
            (None, Some(Entry::Answer(_))) => Activity::Writing,
            _ => Activity::Thinking,
        }
    }

    /// Wraps whatever changed for `width` and returns the total line count.
    pub fn layout(&mut self, width: u16) -> usize {
        if width != self.width {
            self.width = width;
            self.items.iter_mut().for_each(|item| item.lines = None);
        }
        let mut previous: Option<&Entry> = None;
        let mut total = 0;
        for item in &mut self.items {
            if item.lines.is_none() || is_live(&item.entry) {
                let mut lines = Vec::new();
                if needs_gap(previous, &item.entry) {
                    lines.push(Line::default());
                }
                lines.extend(render(&item.entry, &self.cwd, width));
                item.lines = Some(lines);
            }
            total += item.lines.as_ref().map_or(0, Vec::len);
            previous = Some(&item.entry);
        }
        total
    }

    /// The lines in `top..top + height`, as wrapped by the last `layout`.
    pub fn visible(&self, top: usize, height: usize) -> Vec<Line<'static>> {
        self.items
            .iter()
            .flat_map(|item| item.lines.iter().flatten())
            .skip(top)
            .take(height)
            .cloned()
            .collect()
    }

    fn push(&mut self, entry: Entry) {
        self.items.push(Item { entry, lines: None });
    }

    fn last(&self) -> Option<&Entry> {
        self.items.last().map(|item| &item.entry)
    }

    fn close_reasoning(&mut self) {
        if let Some(item) = self.items.last_mut()
            && let Entry::Reasoning { started, took } = &mut item.entry
            && took.is_none()
        {
            *took = Some(started.elapsed());
            item.lines = None;
        }
    }

    fn tool_calls_since_user(&self) -> usize {
        self.entries()
            .rev()
            .take_while(|entry| !matches!(entry, Entry::User(_)))
            .filter(|entry| matches!(entry, Entry::Tool { .. }))
            .count()
    }
}

/// An entry whose line changes without an event, so it is redrawn each frame.
fn is_live(entry: &Entry) -> bool {
    matches!(entry, Entry::Reasoning { took: None, .. })
}

/// One-line rows that stack with no gap between them.
fn is_compact(entry: &Entry) -> bool {
    matches!(
        entry,
        Entry::Reasoning { .. } | Entry::Tool { .. } | Entry::TurnDone { .. }
    )
}

fn needs_gap(previous: Option<&Entry>, entry: &Entry) -> bool {
    match previous {
        None => false,
        Some(_) if matches!(entry, Entry::TurnDone { .. }) => false,
        Some(previous) => !(is_compact(previous) && is_compact(entry)),
    }
}

fn render(entry: &Entry, cwd: &std::path::Path, width: u16) -> Vec<Line<'static>> {
    let dim = Style::new().add_modifier(Modifier::DIM);
    match entry {
        Entry::User(text) => barred(text, width, Style::new().fg(Color::Green), Style::new()),
        Entry::Answer(text) => barred(text, width, Style::new().fg(Color::Magenta), Style::new()),
        Entry::TurnError(e) => {
            let red = Style::new().fg(Color::Red);
            barred(&format!("✗ {e}"), width, red, red)
        }
        Entry::Reasoning { started, took } => {
            let (label, secs) = match took {
                None => ("thinking", started.elapsed()),
                Some(took) => ("thought", *took),
            };
            vec![Line::styled(
                format!("{INDENT}∴ {label} · {:.1}s", secs.as_secs_f64()),
                dim,
            )]
        }
        Entry::Tool { call, state } => {
            let (marker, marker_style, name_style) = match state {
                ToolState::Running => ("▸", dim, Style::new().fg(Color::Cyan)),
                ToolState::Done => (
                    "✓",
                    Style::new().fg(Color::Green),
                    Style::new().fg(Color::Cyan),
                ),
                ToolState::Failed(_) => (
                    "✗",
                    Style::new().fg(Color::Red),
                    Style::new().fg(Color::Red),
                ),
            };
            let mut spans = vec![
                Span::raw(INDENT),
                Span::styled(marker, marker_style),
                Span::raw(" "),
                Span::styled(format!("{:<6} ", call.name), name_style),
                Span::styled(call.summary(cwd), dim),
            ];
            if let ToolState::Failed(e) = state {
                spans.push(Span::styled(format!("  {e}"), Style::new().fg(Color::Red)));
            }
            vec![Line::from(spans)]
        }
        Entry::TurnDone {
            model,
            tool_calls,
            elapsed,
        } => {
            let calls = match tool_calls {
                0 => String::new(),
                1 => " · 1 tool call".to_string(),
                n => format!(" · {n} tool calls"),
            };
            vec![Line::from(vec![
                Span::raw(INDENT),
                Span::styled("✓ ", Style::new().fg(Color::Green)),
                Span::styled(
                    format!("{model}{calls} · {:.1}s", elapsed.as_secs_f64()),
                    dim,
                ),
            ])]
        }
    }
}

/// Wraps `text` to fit beside the message bar, repeating the bar on every
/// line. Blank lines inside the text keep the bar so a block reads as one.
fn barred(text: &str, width: u16, bar: Style, body: Style) -> Vec<Line<'static>> {
    let room = usize::from(width.saturating_sub(BAR_WIDTH).max(1));
    let text = text.trim_matches('\n').replace('\t', "    ");
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.trim_end();
        if raw.is_empty() {
            lines.push(Line::from(Span::styled(BAR, bar)));
            continue;
        }
        // Indented lines (code, nested lists) wrap under their own indent.
        let content = raw.trim_start();
        let indent = &raw[..raw.len() - content.len()];
        let indent = if indent.len() < room { indent } else { "" };
        let options = textwrap::Options::new(room)
            .initial_indent(indent)
            .subsequent_indent(indent);
        for piece in textwrap::wrap(content, options) {
            lines.push(Line::from(vec![
                Span::styled(BAR, bar),
                Span::styled(piece.into_owned(), body),
            ]));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "read".into(),
            arguments: r#"{"filePath":"/repo/src/a.rs"}"#.into(),
        }
    }

    fn transcript() -> Transcript {
        Transcript::new("/repo".into())
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.to_string()).collect()
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
        assert!(matches!(entries[1], Entry::Reasoning { took: Some(_), .. }));
        assert_eq!(
            entries[2],
            &Entry::Tool {
                call: call("1"),
                state: ToolState::Failed("no such file".into())
            }
        );
        assert_eq!(entries[3], &Entry::Answer("done".into()));
        assert!(matches!(entries[4], Entry::TurnDone { tool_calls: 1, .. }));
    }

    #[test]
    fn activity_follows_the_turn() {
        let mut t = transcript();
        t.push_user("go".into());
        assert_eq!(t.activity(), Activity::Thinking);

        t.apply(&Event::ToolStarted(call("1")));
        assert_eq!(t.activity(), Activity::Tool(&call("1")));

        t.apply(&Event::ToolFinished {
            call: call("1"),
            result: Ok("x".into()),
        });
        t.apply(&Event::TextDelta("hi".into()));
        assert_eq!(t.activity(), Activity::Writing);
    }

    #[test]
    fn wraps_under_the_bar_and_separates_blocks() {
        let mut t = transcript();
        t.push_user("one two three".into());
        t.apply(&Event::ToolStarted(call("1")));
        t.apply(&Event::ToolStarted(call("2")));
        t.apply(&Event::TextDelta("ok\n\n    indented".into()));

        let total = t.layout(10);

        assert_eq!(
            text(&t.visible(0, total)),
            [
                "▎ one two",
                "▎ three",
                "",
                "  ▸ read   src/a.rs",
                "  ▸ read   src/a.rs",
                "",
                "▎ ok",
                "▎ ",
                "▎     inde",
                "▎     nted",
            ]
        );
        assert_eq!(text(&t.visible(3, 2)), ["  ▸ read   src/a.rs"; 2]);
    }

    #[test]
    fn rewraps_when_the_width_changes() {
        let mut t = transcript();
        t.push_user("one two three".into());

        assert_eq!(t.layout(10), 2);
        assert_eq!(t.layout(40), 1);
    }
}
