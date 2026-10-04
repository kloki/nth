//! The chat history as a list of entries, built from session events.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use nth_protocol::{Event, Message, ToolCall};
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
        /// What the call produced, at most `OUTPUT_LINES`, kept after it
        /// finishes so it can be read back.
        output: Vec<String>,
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

/// The most output a tool row keeps.
const OUTPUT_LINES: usize = 10;

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

    /// The history of a resumed session, rebuilt by replaying its messages
    /// as the events they once streamed as. Timings are not saved, so turns
    /// get no footers and reasoning shows no duration.
    pub fn replay(cwd: PathBuf, messages: &[Message]) -> Self {
        let mut t = Self::new(cwd);
        for message in messages {
            match message {
                Message::System(_) => {}
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
                        // Its answers come from the result, as they did live.
                        None if call.name == "question" => Ok(content.clone()),
                        None => {
                            // read appends instruction files for the model
                            // only; the live view never showed them.
                            let shown = match content.split_once("\n\n<system-reminder>") {
                                Some((shown, _)) if call.name == "read" => shown,
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
                    entry: Entry::Tool { state, output, .. },
                    lines,
                }) = self.tool_mut(&call.id)
                {
                    // Your answers show under the call; the line above them
                    // is for the model.
                    if let ("question", Ok(answers)) = (call.name.as_str(), result) {
                        output.extend(answers.lines().skip(1).map(String::from));
                    }
                    *state = match result {
                        Ok(_) => ToolState::Done,
                        Err(e) => ToolState::Failed(e.lines().next().unwrap_or_default().into()),
                    };
                    *lines = None;
                }
            }
            Event::Usage(_) => {}
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

    fn push(&mut self, entry: Entry) {
        self.items.push(Item { entry, lines: None });
    }

    fn tool_mut(&mut self, id: &str) -> Option<&mut Item> {
        self.items
            .iter_mut()
            .rev()
            .find(|item| matches!(&item.entry, Entry::Tool { call, .. } if call.id == id))
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
                state: ToolState::Failed("no such file".into()),
                output: Vec::new(),
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
                state: ToolState::Failed("interrupted".into()),
                output: Vec::new(),
            }
        );
        assert!(matches!(entries[4], Entry::Interrupted { .. }));
        let total = t.layout(40);
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
        assert!(matches!(entries[1], Entry::Reasoning { took: Some(_), .. }));
        assert_eq!(entries[2], &Entry::Answer("looking".into()));
        assert_eq!(
            entries[3],
            &Entry::Tool {
                call: bash,
                state: ToolState::Done,
                output: vec!["a.rs".into(), "b.rs".into()],
            }
        );
        assert_eq!(
            entries[4],
            &Entry::Tool {
                call: call("2"),
                state: ToolState::Failed("no such file".into()),
                output: Vec::new(),
            }
        );
        assert_eq!(entries[5], &Entry::Answer("done".into()));
        assert_eq!(entries.len(), 6);
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
