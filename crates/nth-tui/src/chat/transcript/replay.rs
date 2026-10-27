//! Rebuild a transcript from saved session messages.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use nth_protocol::{Event, Message, failure_reason, tool_names::writes_files};
use nth_session::SHELL_PROMPT;

use super::{Entry, Item, Transcript};

impl Transcript {
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
                Message::Compaction { summary, .. } => {
                    t.push(Entry::Compacted(summary.clone()));
                }
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
                Message::ToolResult {
                    call_id, content, ..
                } => {
                    let Some(Item {
                        entry: Entry::Tool { call, .. },
                        ..
                    }) = t.tool_mut(call_id)
                    else {
                        continue;
                    };
                    let call = call.clone();
                    let result = match failure_reason(content) {
                        Some(error) => Err(error.to_string()),
                        // A write streamed nothing; its start showed its content.
                        None if call.name == "write" => Ok(content.clone()),
                        // A skill is one row; its body is for the model.
                        None if call.name == "skill" => Ok(content.clone()),
                        // A question is one row too; the answers are for the model.
                        None if call.name == "question" => Ok(content.clone()),
                        // The panel or file it showed says what it did.
                        None if call.name == "panel" || call.name == "show_user" => {
                            Ok(content.clone())
                        }
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
}
