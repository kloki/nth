//! The chat history as a list of entries, built from session events.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use nth_protocol::{Event, ToolCall};
use ratatui::text::Line;

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
    /// Footer of a turn the user stopped with Esc.
    Interrupted {
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
    pub(super) cwd: PathBuf,
    pub(super) items: Vec<Item>,
    /// Width the cached lines were wrapped for.
    pub(super) width: u16,
}

pub(super) struct Item {
    pub(super) entry: Entry,
    /// Wrapped lines, including the blank line that separates this entry
    /// from the previous one. Dropped whenever the entry or width changes.
    pub(super) lines: Option<Vec<Line<'static>>>,
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

    /// Closes a turn the user cut short. Tools still running never report
    /// back, so they are marked as stopped here.
    pub fn interrupt(&mut self, elapsed: Duration) {
        self.close_reasoning();
        for item in &mut self.items {
            if let Entry::Tool { state, .. } = &mut item.entry
                && *state == ToolState::Running
            {
                *state = ToolState::Failed("interrupted".into());
                item.lines = None;
            }
        }
        self.push(Entry::Interrupted { elapsed });
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
                state: ToolState::Failed("interrupted".into())
            }
        );
        assert!(matches!(entries[4], Entry::Interrupted { .. }));
        let total = t.layout(40);
        assert_eq!(text(&t.visible(total - 1, 1)), ["  ⏹ interrupted · 3.0s"]);
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
}
