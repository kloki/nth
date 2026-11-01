//! Read-only, windowed document for the next-request preview.

use std::fmt::Write;

use nth_protocol::Message;
use nth_session::context_preview::{Entry, Preview};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    pager::{Cache, Pager},
    rich,
};

#[derive(Debug, Default)]
pub(crate) struct ContextView {
    text: Option<String>,
    pub pager: Pager,
    cache: Cache,
}

impl ContextView {
    /// Replace the formatted snapshot without moving the reader.
    pub(crate) fn update(&mut self, text: Option<String>) {
        if self.text != text {
            self.text = text;
            self.cache.invalidate();
        }
    }

    pub(crate) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let width = usize::from(area.width).max(1);
        let lines = self.cache.take(width).unwrap_or_else(|| {
            self.text
                .as_deref()
                .unwrap_or("Next-request preview\n\nWaiting for context preview.")
                .split('\n')
                .flat_map(|line| {
                    let line = if let Some(bar) = line.strip_prefix("Budget bar: ") {
                        let mut spans = vec![Span::raw("Budget bar: ")];
                        for ch in bar.chars() {
                            let colour = match ch {
                                '■' => Color::Cyan,
                                '▒' => Color::Yellow,
                                _ => Color::DarkGray,
                            };
                            spans.push(Span::styled(ch.to_string(), Style::default().fg(colour)));
                        }
                        Line::from(spans)
                    } else {
                        Line::raw(line.to_owned())
                    };
                    rich::wrap(line, width)
                })
                .collect()
        });
        let top = self.pager.layout(lines.len(), usize::from(area.height));
        let visible = lines
            .iter()
            .skip(top)
            .take(usize::from(area.height))
            .cloned()
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(visible), area);
        self.cache.keep(width, lines);
    }
}

/// Format a logical preview without ever serializing image bytes.
pub(crate) fn format(preview: &Preview) -> String {
    let mut text = String::new();
    let budget = &preview.budget;
    let _ = writeln!(
        text,
        "Next-request preview · {} · {} · {}",
        preview.model,
        preview.agent.as_deref().unwrap_or(preview.mode.label()),
        preview.effort.name()
    );
    let _ = writeln!(
        text,
        "Logical request · byte-based estimates; provider accounting may differ"
    );
    let _ = writeln!(
        text,
        "Estimated input ~{} / {} context",
        budget.input,
        known(budget.context_window)
    );
    if let Some(window) = budget.context_window.filter(|&n| n > 0) {
        let input = ((budget.input as u128 * 20 / window as u128).min(20)) as usize;
        let reserve = ((budget.output_reserve as u128 * 20 / window as u128)
            .min((20 - input) as u128)) as usize;
        let _ = writeln!(
            text,
            "Budget bar: {}{}{}  input / reserve / available",
            "■".repeat(input),
            "▒".repeat(reserve),
            "·".repeat(20 - input - reserve)
        );
    }
    let _ = writeln!(
        text,
        "\n  System prompt       ~{}\n  Conversation        ~{}\n  Tool definitions    ~{}\n  Images              ~{}",
        budget.system, budget.conversation, budget.tools, budget.images
    );
    let _ = writeln!(
        text,
        "  Output reserve       {} · usable {} · available {}",
        budget.output_reserve,
        known(budget.usable),
        known(budget.available)
    );
    if let Some(usage) = preview.last_usage {
        let _ = writeln!(
            text,
            "\nLast reported input: {} · cached: {}",
            usage.input,
            known(usage.cache_read)
        );
    }
    if preview.phase != "idle" {
        let _ = writeln!(text, "\n{}", preview.phase.replace('_', " "));
    }
    if preview.compaction_required {
        let _ = writeln!(text, "Compaction required before next request");
    }
    for (kind, value) in &preview.pending {
        let _ = writeln!(text, "Pending {kind}: {value}");
    }
    let _ = writeln!(text, "\nSystem prompt");
    for entry in preview
        .entries
        .iter()
        .filter(|e| matches!(e.message, Message::System(_)))
    {
        entry_text(&mut text, entry);
    }
    let count = preview
        .entries
        .iter()
        .filter(|e| !matches!(e.message, Message::System(_)))
        .count();
    let _ = writeln!(text, "\nConversation · {count} messages");
    for entry in preview
        .entries
        .iter()
        .filter(|e| !matches!(e.message, Message::System(_)))
    {
        entry_text(&mut text, entry);
    }
    let _ = writeln!(text, "\nTool definitions · {} tools", preview.tools.len());
    for tool in &preview.tools {
        let schema = serde_json::to_string_pretty(&tool.parameters).unwrap_or_default();
        let _ = writeln!(
            text,
            "\n{} · ~{} tokens\n{}\nSchema:\n{schema}",
            tool.name, tool.estimated_tokens, tool.description
        );
    }
    text
}

fn known(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".into(), |value| value.to_string())
}

fn entry_text(out: &mut String, entry: &Entry) {
    let checkpoint = if entry.checkpoint {
        " · checkpoint"
    } else {
        ""
    };
    let tokens = entry.estimated_tokens;
    match &entry.message {
        Message::System(text) => {
            let _ = writeln!(out, "\nSystem{checkpoint} · ~{tokens} tokens\n{text}");
        }
        Message::User(text) => {
            let _ = writeln!(out, "\nUser{checkpoint} · ~{tokens} tokens\n{text}");
        }
        Message::Assistant(message) => {
            let _ = writeln!(out, "\nAssistant{checkpoint} · ~{tokens} tokens");
            if !message.reasoning.is_empty() {
                let _ = writeln!(out, "Reasoning:\n{}", message.reasoning);
            }
            if !message.text.is_empty() {
                let _ = writeln!(out, "{}", message.text);
            }
            for call in &message.tool_calls {
                let _ = writeln!(
                    out,
                    "Tool call {} · {}\n{}",
                    call.name, call.id, call.arguments
                );
            }
        }
        Message::ToolResult {
            call_id,
            content,
            images,
        } => {
            let _ = writeln!(
                out,
                "\nTool result · {call_id}{checkpoint} · ~{tokens} tokens\n{content}"
            );
            for (index, image) in images.iter().enumerate() {
                let available = if entry.images_available.get(index).copied().unwrap_or(false) {
                    "available"
                } else {
                    "unavailable"
                };
                let _ = writeln!(
                    out,
                    "Image: {} · {} · {}x{} · {available} · ~2500 tokens",
                    image.path.display(),
                    image.mime,
                    image.width,
                    image.height
                );
            }
        }
        Message::Compaction { summary, recent } => {
            let _ = writeln!(out, "\nCheckpoint · ~{tokens} tokens\n{summary}\n{recent}");
        }
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::{AssistantMessage, Inbox, ToolCall};
    use nth_session::{Session, context_preview::ToolDefinition};

    use super::*;

    #[test]
    fn document_includes_full_request_in_order() {
        let mut session = Session::new("model", ".".into());
        session.messages = vec![
            Message::System("system body".into()),
            Message::User("user body".into()),
            Message::Assistant(AssistantMessage {
                text: "answer body".into(),
                reasoning: "reasoning body".into(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "read".into(),
                    arguments: "{\"path\":\"file\"}".into(),
                }],
            }),
            Message::ToolResult {
                call_id: "call-1".into(),
                content: "result body".into(),
                images: vec![],
            },
        ];
        let mut preview = session.context_preview(&[], &Inbox::default());
        preview.tools.push(ToolDefinition {
            name: "read".into(),
            description: "read description".into(),
            parameters: serde_json::json!({"type": "object"}),
            estimated_tokens: 8,
        });
        let text = format(&preview);
        let parts = [
            "Next-request preview",
            "System prompt",
            "system body",
            "Conversation · 3 messages",
            "user body",
            "reasoning body",
            "answer body",
            "Tool call read · call-1",
            "result body",
            "Tool definitions · 1 tools",
            "read description",
            "\"type\": \"object\"",
        ];
        let mut cursor = 0;
        for part in parts {
            let at = text[cursor..]
                .find(part)
                .unwrap_or_else(|| panic!("missing {part:?} in {text}"));
            cursor += at + part.len();
        }
    }
}
