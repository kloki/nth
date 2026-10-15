//! Compaction: summarizing everything before a marked point so a long
//! session keeps fitting the model's context window. The history is marked,
//! never rewritten: a [`Message::Compaction`] is spliced in before the
//! verbatim tail, and later requests start at it (the effective messages in
//! `agent_loop::stream`).
//!
//! This module holds the policy — the [`Compaction`] config and its
//! defaults, the tail selection and the head serialization the summary
//! request is built from. Running one is `Turn::compact`.

use nth_protocol::Message;
use serde::{Deserialize, Serialize};

/// The prompt the summary request carries, before the serialized head.
pub const SUMMARY_PROMPT: &str = include_str!("prompts/compaction/summary.md");

/// What an effective conversation asks once a compaction has landed; the
/// summary is the model's answer to it.
pub const WHAT_DID_WE_DO: &str = "What did we do so far?";

/// The buffer kept below the window for a reply's own tokens, when neither
/// the config nor the model's output limit says otherwise.
const DEFAULT_RESERVED: u64 = 20_000;

/// The smallest and largest verbatim tail a compaction keeps.
const MIN_PRESERVE: u64 = 2_000;
const MAX_PRESERVE: u64 = 15_000;

/// The most of any one tool result the summary prompt is shown.
const TOOL_RESULT_MAX_CHARS: usize = 2_000;

/// How a session keeps itself inside the model's window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Compaction {
    /// Whether a reply that reaches the usable window is compacted before
    /// the turn's next step.
    #[serde(default = "default_auto")]
    pub auto: bool,
    /// The buffer kept below the window; `0` means `min(20_000, output)`.
    #[serde(default)]
    pub reserved: u64,
    /// Tokens a compaction keeps verbatim; `0` means a quarter of the
    /// usable window, clamped to 2_000–15_000.
    #[serde(default)]
    pub preserve: u64,
}

impl Default for Compaction {
    fn default() -> Self {
        Self::DEFAULT
    }
}

fn default_auto() -> bool {
    true
}

impl Compaction {
    /// The defaults, for the const routes tests build.
    pub const DEFAULT: Compaction = Compaction {
        auto: true,
        reserved: 0,
        preserve: 0,
    };

    /// The buffer kept below the window: the configured one, or
    /// `min(20_000, output)` by default.
    pub fn reserved(&self, output: Option<u64>) -> u64 {
        match self.reserved {
            0 => output.unwrap_or(DEFAULT_RESERVED).min(DEFAULT_RESERVED),
            reserved => reserved,
        }
    }

    /// The tokens a conversation may fill before compaction fires: the
    /// window less the reserved buffer. `None` when no window is known, in
    /// which case nothing fires on its own.
    pub fn usable(&self, context_window: Option<u64>, output: Option<u64>) -> Option<u64> {
        Some(context_window?.saturating_sub(self.reserved(output)))
    }

    /// The verbatim tail budget under a usable window: the configured one,
    /// or a quarter of the window clamped to 2_000–15_000; the largest
    /// default when there is no window to size it against.
    pub fn preserve(&self, usable: Option<u64>) -> u64 {
        if self.preserve > 0 {
            return self.preserve;
        }
        match usable {
            Some(usable) => (usable / 4).clamp(MIN_PRESERVE, MAX_PRESERVE),
            None => MAX_PRESERVE,
        }
    }
}

/// Where the verbatim tail of `messages` starts: whole turns, newest first,
/// while their sizes fit `preserve` tokens; the first turn that does not fit
/// is split at the earliest message from which the rest fits. A turn is the
/// messages after one `Message::User` up to the next. A split starts at a
/// user or assistant message, so the tail never opens on a tool result
/// whose call was summarized away; when no part of the turn fits, the turn
/// stays entirely in the head, as opencode leaves it. `messages.len()` when
/// no tail is kept at all.
pub(crate) fn tail_start(messages: &[Message], preserve: u64) -> usize {
    let starts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| matches!(message, Message::User(_)))
        .map(|(at, _)| at)
        .collect();
    let mut tail = messages.len();
    let mut used = 0;
    for (n, &start) in starts.iter().enumerate().rev() {
        let end = starts.get(n + 1).copied().unwrap_or(messages.len());
        let size: u64 = messages[start..end].iter().map(tokens).sum();
        if used + size <= preserve {
            used += size;
            tail = start;
            continue;
        }
        if let Some(split) = split(&messages[start..end], preserve.saturating_sub(used)) {
            tail = start + split;
        }
        break;
    }
    tail
}

/// Where in a turn the tail starts when the turn does not fit whole: the
/// earliest user or assistant message whose suffix fits `budget`. `None`
/// when none does — never the turn's first message, which would keep the
/// whole turn that already did not fit.
fn split(turn: &[Message], budget: u64) -> Option<usize> {
    if budget == 0 || turn.len() <= 1 {
        return None;
    }
    let mut suffix = 0;
    let mut fitting = None;
    for at in (1..turn.len()).rev() {
        suffix += tokens(&turn[at]);
        if matches!(turn[at], Message::User(_) | Message::Assistant(_)) && suffix <= budget {
            fitting = Some(at);
        }
    }
    fitting
}

/// The earlier conversation as the lines the summary prompt is given, one
/// per message, tool results cut to 2_000 chars: `[User]: …`,
/// `[Assistant]: …`, `[Assistant tool call]: <name> <arguments>`,
/// `[Tool result]: …`, and `[Summary]: …` for an earlier compaction, so a
/// repeated one merges into its shape rather than drifting.
pub(crate) fn serialize_head(messages: &[Message]) -> String {
    let mut lines = Vec::new();
    for message in messages {
        match message {
            // The system prompt is not the conversation; every request
            // carries it anyway.
            Message::System(_) => {}
            Message::User(text) => lines.push(format!("[User]: {text}")),
            Message::Assistant(reply) => {
                if !reply.text.trim().is_empty() {
                    lines.push(format!("[Assistant]: {}", reply.text));
                }
                lines.extend(reply.tool_calls.iter().map(|call| {
                    format!("[Assistant tool call]: {} {}", call.name, call.arguments)
                }));
            }
            Message::ToolResult { content, .. } => {
                lines.push(format!(
                    "[Tool result]: {}",
                    cut(content, TOOL_RESULT_MAX_CHARS)
                ));
            }
            Message::Compaction { summary } => lines.push(format!("[Summary]: {summary}")),
        }
    }
    lines.join("\n")
}

/// The first `max` chars of `text`, cut at a char boundary.
fn cut(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}

/// A message's size in tokens, estimated as its serialized chars / 4, as
/// opencode does.
fn tokens(message: &Message) -> u64 {
    serde_json::to_string(message).map_or(0, |json| json.len() as u64) / 4
}

#[cfg(test)]
mod tests {
    use nth_protocol::{AssistantMessage, ToolCall};

    use super::*;

    fn user(text: &str) -> Message {
        Message::User(text.into())
    }

    fn answer(text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            text: text.into(),
            ..AssistantMessage::default()
        })
    }

    fn result(content: &str) -> Message {
        Message::ToolResult {
            call_id: "1".into(),
            content: content.into(),
        }
    }

    #[test]
    fn the_defaults_follow_the_model_limits() {
        let compaction = Compaction::default();
        assert!(compaction.auto);
        // min(20k, output).
        assert_eq!(compaction.reserved(None), 20_000);
        assert_eq!(compaction.reserved(Some(8_192)), 8_192);
        assert_eq!(
            compaction.usable(Some(200_000), Some(128_000)),
            Some(180_000)
        );
        assert_eq!(compaction.usable(None, None), None);
        // A quarter of the usable window, clamped.
        assert_eq!(compaction.preserve(Some(1_000_000)), 15_000);
        assert_eq!(compaction.preserve(Some(40_000)), 10_000);
        assert_eq!(compaction.preserve(Some(4_000)), 2_000);
        assert_eq!(compaction.preserve(None), 15_000);

        let configured = Compaction {
            auto: false,
            reserved: 1_000,
            preserve: 500,
        };
        assert_eq!(configured.reserved(None), 1_000);
        assert_eq!(configured.preserve(Some(1_000_000)), 500);
    }

    #[test]
    fn the_tail_keeps_whole_recent_turns() {
        let messages = vec![
            Message::System("prompt".into()),
            user("old"),
            answer("a"),
            user("new"),
            answer("b"),
        ];
        let new_turn: u64 = [user("new"), answer("b")].iter().map(tokens).sum();

        assert_eq!(tail_start(&messages, new_turn), 3, "only the newest fits");
        assert_eq!(tail_start(&messages, 0), 5, "nothing fits: no tail at all");
        let all: u64 = messages.iter().map(tokens).sum();
        assert_eq!(tail_start(&messages, all), 1, "everything fits");
    }

    #[test]
    fn a_turn_that_does_not_fit_is_split_at_the_earliest_message_that_does() {
        // One turn: the prompt, a huge tool exchange, then the answer.
        let messages = vec![
            Message::System("prompt".into()),
            user("go"),
            Message::Assistant(AssistantMessage {
                tool_calls: vec![ToolCall {
                    id: "1".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                }],
                ..AssistantMessage::default()
            }),
            result(&"x".repeat(8_000)),
            answer("done"),
        ];
        let answer_only = tokens(&messages[4]);
        let exchange: u64 = messages[2..].iter().map(tokens).sum();

        // The whole turn does not fit, but the answer alone does: the tail
        // starts at the answer, never on the tool result before it.
        assert_eq!(tail_start(&messages, exchange - 1), 4);
        // Room for the exchange: the tail starts at the call that opened it.
        assert_eq!(tail_start(&messages, exchange), 2);
        // Not even the answer fits: nothing of the turn is kept.
        assert_eq!(tail_start(&messages, answer_only - 1), messages.len());
    }

    #[test]
    fn head_lines_name_who_spoke_and_cut_tool_results() {
        let long = "x".repeat(3_000);
        let messages = vec![
            Message::System("you are nth".into()),
            user("go"),
            Message::Assistant(AssistantMessage {
                text: "reading".into(),
                reasoning: "hmm".into(),
                tool_calls: vec![ToolCall {
                    id: "1".into(),
                    name: "read".into(),
                    arguments: r#"{"filePath":"a.rs"}"#.into(),
                }],
            }),
            result(&long),
            Message::Compaction {
                summary: "earlier".into(),
            },
        ];

        let head = serialize_head(&messages);
        let lines: Vec<&str> = head.lines().collect();
        assert_eq!(lines.len(), 5, "the system prompt is left out");
        assert_eq!(lines[0], "[User]: go");
        assert_eq!(lines[1], "[Assistant]: reading");
        assert_eq!(
            lines[2],
            r#"[Assistant tool call]: read {"filePath":"a.rs"}"#
        );
        assert_eq!(
            lines[3].chars().count(),
            "[Tool result]: ".chars().count() + 2_000,
            "results are cut"
        );
        assert!(lines[3].ends_with(&"x".repeat(100)));
        assert_eq!(
            lines[3]["[Tool result]: ".len()..]
                .chars()
                .filter(|c| *c == 'x')
                .count(),
            2_000
        );
        assert_eq!(lines[4], "[Summary]: earlier");
    }
}
