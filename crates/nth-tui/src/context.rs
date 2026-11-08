//! Read-only, windowed document for the next-request preview: the budget
//! summary, the system prompt and the offered tool definitions, in grey
//! barred sections, without the conversation itself.

use std::sync::Arc;

use nth_protocol::{Budget, Message, Phase, Preview};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{
    pager::{Cache, Pager},
    rich, theme,
};

#[derive(Debug, Default)]
pub(crate) struct ContextView {
    preview: Option<Arc<Preview>>,
    pub pager: Pager,
    cache: Cache,
}

impl ContextView {
    /// Replace the preview without moving the reader.
    pub(crate) fn update(&mut self, preview: Option<Arc<Preview>>) {
        if self.preview != preview {
            self.preview = preview;
            self.cache.invalidate();
        }
    }

    #[cfg(test)]
    pub(crate) fn preview(&self) -> Option<&Preview> {
        self.preview.as_deref()
    }

    pub(crate) fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let width = usize::from(area.width).max(1);
        let lines = self
            .cache
            .take(width)
            .unwrap_or_else(|| match &self.preview {
                Some(preview) => document(preview, width),
                None => waiting(width),
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

/// The summary, then the actual context sent: the system prompt and the
/// tool definitions. The conversation is left out; only its cost is summed.
fn document(preview: &Preview, width: usize) -> Vec<Line<'static>> {
    let mut blocks = vec![summary(preview)];
    if !preview.pending.is_empty() {
        blocks.push(pending(preview));
    }
    blocks.push(prompt(preview));
    blocks.push(tool_definitions(preview));
    barred(blocks, width)
}

fn waiting(width: usize) -> Vec<Line<'static>> {
    barred(
        vec![vec![title("context"), note("waiting for the running turn")]],
        width,
    )
}

/// Wrap each line for the room beside the section bar, then put the bar on
/// every row, so a long wrapped line keeps it on each of its rows.
fn barred(blocks: Vec<Vec<Line<'static>>>, width: usize) -> Vec<Line<'static>> {
    let text_width = width.saturating_sub(usize::from(theme::BAR_WIDTH)).max(1);
    theme::sections(
        blocks
            .into_iter()
            .map(|block| {
                block
                    .into_iter()
                    .flat_map(|line| rich::wrap(line, text_width))
                    .collect()
            })
            .collect(),
    )
}

/// What the next request costs, never what it says.
fn summary(preview: &Preview) -> Vec<Line<'static>> {
    let budget = &preview.budget;
    let mut lines = vec![title("summary")];
    lines.push(Line::from(vec![
        Span::raw(theme::INDENT),
        Span::styled(preview.model.clone(), Style::new().fg(Color::Blue)),
        Span::styled(
            format!(
                " · {} · {}",
                preview.agent.as_deref().unwrap_or(preview.mode.label()),
                preview.effort.name()
            ),
            theme::dim(),
        ),
    ]));
    if preview.phase != Phase::Idle {
        lines.push(field("state", preview.phase.label().into()));
    }
    lines.push(note(
        "logical request · byte-based estimates; provider accounting may differ",
    ));
    lines.push(field(
        "input",
        format!(
            "~{} / {} context",
            budget.input,
            known(budget.context_window)
        ),
    ));
    if let Some(window) = budget.context_window.filter(|&window| window > 0) {
        lines.push(budget_bar(budget, window));
    }
    for (label, tokens) in [
        ("system prompt", budget.system),
        ("conversation", budget.conversation),
        ("tool definitions", budget.tools),
        ("images", budget.images),
    ] {
        lines.push(field(label, format!("~{tokens}")));
    }
    lines.push(field(
        "output reserve",
        format!(
            "{} · usable {} · available {}",
            budget.output_reserve,
            known(budget.usable),
            known(budget.available)
        ),
    ));
    if let Some(usage) = preview.last_usage {
        lines.push(field(
            "last reported input",
            format!("{} · cached {}", usage.input, known(usage.cache_read)),
        ));
    }
    if preview.compaction_required {
        lines.push(Line::from(vec![
            Span::raw(theme::INDENT),
            Span::styled(
                "compaction required before next request",
                Style::new().fg(Color::Yellow),
            ),
        ]));
    }
    lines
}

/// What is about to join the request but has not been sent yet.
fn pending(preview: &Preview) -> Vec<Line<'static>> {
    let mut lines = vec![title("pending")];
    for (kind, value) in &preview.pending {
        lines.push(Line::from(vec![
            Span::raw(theme::INDENT),
            Span::styled(kind.clone(), Style::new().fg(Color::Yellow)),
        ]));
        text_lines(&mut lines, value);
    }
    lines
}

/// The system prompt as sent, with its instructions, memory, skills and
/// agents in the words the model reads.
fn prompt(preview: &Preview) -> Vec<Line<'static>> {
    let entries = preview
        .entries
        .iter()
        .filter(|entry| matches!(entry.message, Message::System(_)))
        .collect::<Vec<_>>();
    let tokens = entries
        .iter()
        .map(|entry| entry.estimated_tokens)
        .sum::<u64>();
    let mut lines = vec![heading(
        "context · system prompt",
        format!("~{tokens} tokens"),
    )];
    if entries.is_empty() {
        lines.push(note("none"));
    }
    for entry in entries {
        if let Message::System(text) = &entry.message {
            text_lines(&mut lines, text);
        }
    }
    lines
}

/// The tools the model may call, with their schemas.
fn tool_definitions(preview: &Preview) -> Vec<Line<'static>> {
    let tokens = preview
        .tools
        .iter()
        .map(|tool| tool.estimated_tokens)
        .sum::<u64>();
    let mut lines = vec![heading(
        "context · tool definitions",
        format!("{} tools · ~{tokens} tokens", preview.tools.len()),
    )];
    if preview.tools.is_empty() {
        lines.push(note("none"));
        return lines;
    }
    for (index, tool) in preview.tools.iter().enumerate() {
        if index > 0 {
            lines.push(Line::default());
        }
        lines.push(Line::from(vec![
            Span::raw(theme::INDENT),
            Span::styled(tool.name.clone(), Style::new().fg(Color::Cyan)),
            Span::styled(format!("  ~{} tokens", tool.estimated_tokens), theme::dim()),
        ]));
        if !tool.description.is_empty() {
            text_lines(&mut lines, &tool.description);
        }
        lines.push(Line::from(vec![
            Span::raw(theme::INDENT),
            Span::styled("schema", theme::dim()),
        ]));
        text_lines(
            &mut lines,
            &serde_json::to_string_pretty(&tool.parameters).unwrap_or_default(),
        );
    }
    lines
}

/// One source line of text, under the section's title.
fn text_lines(lines: &mut Vec<Line<'static>>, text: &str) {
    for line in text.split('\n') {
        lines.push(Line::from(vec![
            Span::raw(theme::INDENT),
            Span::raw(line.to_owned()),
        ]));
    }
}

/// The input, reserve and available shares of the context window, twenty
/// cells wide, each part in its own colour.
fn budget_bar(budget: &Budget, window: u64) -> Line<'static> {
    let input = ((budget.input as u128 * 20 / window as u128).min(20)) as usize;
    let reserve =
        ((budget.output_reserve as u128 * 20 / window as u128).min((20 - input) as u128)) as usize;
    Line::from(vec![
        Span::raw(theme::INDENT),
        Span::styled("■".repeat(input), Style::new().fg(Color::Cyan)),
        Span::styled("▒".repeat(reserve), Style::new().fg(Color::Yellow)),
        Span::styled(
            "·".repeat(20 - input - reserve),
            Style::new().fg(Color::DarkGray),
        ),
        Span::styled("  input / reserve / available", theme::dim()),
    ])
}

fn known(value: Option<u64>) -> String {
    value.map_or_else(|| "unknown".into(), |value| value.to_string())
}

/// A summary row: a dim label in its column, then the value.
fn field(label: &str, value: String) -> Line<'static> {
    const LABEL_WIDTH: usize = 17;
    Line::from(vec![
        Span::raw(theme::INDENT),
        Span::styled(format!("{label:<LABEL_WIDTH$}"), theme::dim()),
        Span::raw(value),
    ])
}

fn title(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        text.to_owned(),
        Style::new().add_modifier(Modifier::BOLD),
    ))
}

/// A section's title with a dim count beside it.
fn heading(text: &str, detail: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(text.to_owned(), Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(format!("  {detail}"), theme::dim()),
    ])
}

fn note(text: impl Into<String>) -> Line<'static> {
    Line::from(vec![
        Span::raw(theme::INDENT),
        Span::styled(text.into(), theme::dim()),
    ])
}

#[cfg(test)]
mod tests {
    use nth_protocol::{AssistantMessage, Inbox, ToolCall, ToolDefinition};
    use nth_session::Session;

    use super::*;

    fn rendered(preview: &Preview, width: usize) -> String {
        document(preview, width)
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn document_shows_summary_prompt_and_tools_but_not_messages() {
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
        let text = rendered(&preview, 80);
        for part in [
            "summary",
            "context · system prompt",
            "system body",
            "context · tool definitions",
            "read description",
            "\"type\": \"object\"",
        ] {
            assert!(text.contains(part), "missing {part:?} in {text}");
        }
        for part in [
            "user body",
            "reasoning body",
            "answer body",
            "result body",
            "Tool call",
        ] {
            assert!(!text.contains(part), "leaked {part:?} in {text}");
        }
    }

    #[test]
    fn summary_reports_the_budget_with_a_coloured_bar() {
        let mut session = Session::new("model", ".".into());
        session.set_limits(Some(5_000), Some(1_000));
        let preview = session.context_preview(&[], &Inbox::default());
        let lines = document(&preview, 80);
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("system prompt"), "{text}");
        assert!(text.contains("conversation"), "{text}");
        let bar = lines
            .iter()
            .find(|line| line.to_string().contains("input / reserve / available"))
            .expect("budget bar");
        let colour = |cell: char| {
            bar.spans
                .iter()
                .find(|span| span.content.contains(cell))
                .and_then(|span| span.style.fg)
        };
        assert_eq!(colour('■'), Some(Color::Cyan));
        assert_eq!(colour('▒'), Some(Color::Yellow));
        assert_eq!(colour('·'), Some(Color::DarkGray));
    }

    #[test]
    fn a_long_prompt_wraps_with_a_bar_on_every_row() {
        let mut session = Session::new("model", ".".into());
        session.messages = vec![Message::System("word ".repeat(120))];
        let preview = session.context_preview(&[], &Inbox::default());
        let lines = document(&preview, 20);
        let mut rows = 0;
        for line in &lines {
            if line.spans.is_empty() {
                continue;
            }
            assert_eq!(line.spans[0].content, theme::BAR, "{line}");
            assert_eq!(
                line.spans[0].style.fg,
                Some(theme::SECTION_COLOUR),
                "{line}"
            );
            rows += 1;
        }
        assert!(rows > 30, "the prompt wrapped over rows: {rows}");
    }
}
