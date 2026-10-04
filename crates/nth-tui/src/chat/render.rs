//! Wraps transcript entries into styled lines for the current width, and
//! caches them so only what changed is wrapped again.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};

use super::transcript::{Entry, ToolState, Transcript};
use crate::theme::{BAR, BAR_WIDTH, INDENT, dim};

impl Transcript {
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
                // Every entry stands apart from the one before it.
                if previous.is_some() {
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
}

/// An entry whose line changes without an event, so it is redrawn each frame.
fn is_live(entry: &Entry) -> bool {
    matches!(entry, Entry::Reasoning { took: None, .. })
}

/// Tool output is indented to sit under the tool's name, so it reads as
/// coming from the row above rather than as a message of its own.
const OUTPUT_INDENT: &str = "    ";

fn render(entry: &Entry, cwd: &std::path::Path, width: u16) -> Vec<Line<'static>> {
    let dim = dim();
    match entry {
        Entry::User(text) => barred(text, width, Style::new().fg(Color::Green), Style::new()),
        Entry::Answer(text) => barred(text, width, Style::new().fg(Color::Cyan), Style::new()),
        Entry::TurnError(e) => {
            let red = Style::new().fg(Color::Red);
            barred(&format!("✗ {e}"), width, red, red)
        }
        Entry::Reasoning { started, took } => {
            let text = match took {
                None => format!("thinking · {:.1}s", started.elapsed().as_secs_f64()),
                // A resumed session's reasoning; how long it took isn't saved.
                Some(took) if took.is_zero() => "thought".to_string(),
                Some(took) => format!("thought · {:.1}s", took.as_secs_f64()),
            };
            vec![Line::styled(format!("{INDENT}∴ {text}"), dim)]
        }
        Entry::Tool {
            call,
            state,
            output,
        } => {
            // A call is told apart by its tool's icon, not by a success
            // mark: only a failure stands out, in red.
            let name_style = match state {
                ToolState::Failed(_) => Style::new().fg(Color::Red),
                ToolState::Running | ToolState::Done => Style::new().fg(Color::Cyan),
            };
            let icon_style = match state {
                ToolState::Running => dim,
                ToolState::Done | ToolState::Failed(_) => name_style,
            };
            let mut spans = vec![
                Span::styled(BAR, Style::new().fg(Color::Cyan)),
                Span::styled(icon(&call.name), icon_style),
                Span::raw(" "),
                Span::styled(format!("{:<6} ", call.name), name_style),
                Span::styled(call.summary(cwd), dim),
            ];
            if let ToolState::Failed(e) = state {
                spans.push(Span::styled(format!("  {e}"), Style::new().fg(Color::Red)));
            }
            let mut lines = vec![Line::from(spans)];
            lines.extend(output.iter().map(|text| {
                Line::styled(
                    format!("{OUTPUT_INDENT}{text}"),
                    Style::new().fg(Color::Gray),
                )
            }));
            lines
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
                // Closes the turn as `∴` opens its thinking.
                Span::styled(
                    format!("∎ {model}{calls} · {:.1}s", elapsed.as_secs_f64()),
                    dim,
                ),
            ])]
        }
        Entry::Interrupted { elapsed } => vec![Line::from(vec![
            Span::raw(INDENT),
            Span::styled("⏹ ", Style::new().fg(Color::Yellow)),
            Span::styled(format!("interrupted · {:.1}s", elapsed.as_secs_f64()), dim),
        ])],
    }
}

/// The mark a tool's row starts with, so calls can be told apart at a glance.
fn icon(tool: &str) -> &'static str {
    match tool {
        "read" => "≡",
        "write" => "✎",
        "bash" => "$",
        _ => "•",
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
    use nth_protocol::Event;

    use crate::chat::transcript::tests::{call, text, transcript};

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
                "▎ ≡ read   src/a.rs",
                "",
                "▎ ≡ read   src/a.rs",
                "",
                "▎ ok",
                "▎ ",
                "▎     inde",
                "▎     nted",
            ]
        );
        assert_eq!(
            text(&t.visible(3, 3)),
            ["▎ ≡ read   src/a.rs", "", "▎ ≡ read   src/a.rs"]
        );
    }

    #[test]
    fn the_turn_footer_sits_apart_from_the_turn() {
        let mut t = transcript();
        t.push_user("go".into());
        t.apply(&Event::ToolStarted(call("1")));
        t.finish_turn(Ok(()), "glm", std::time::Duration::from_secs(2));

        let total = t.layout(40);

        assert_eq!(
            text(&t.visible(0, total)),
            [
                "▎ go",
                "",
                "▎ ≡ read   src/a.rs",
                "",
                "  ∎ glm · 1 tool call · 2.0s",
            ]
        );
    }

    #[test]
    fn tool_output_sits_under_its_row() {
        let mut t = transcript();
        t.apply(&Event::ToolStarted(call("1")));
        t.apply(&Event::ToolOutput {
            call_id: "1".into(),
            text: "fn main() {}\n".into(),
        });
        t.apply(&Event::ToolStarted(call("2")));

        let total = t.layout(40);

        assert_eq!(
            text(&t.visible(0, total)),
            [
                "▎ ≡ read   src/a.rs",
                "    fn main() {}",
                "",
                "▎ ≡ read   src/a.rs",
            ]
        );
    }

    #[test]
    fn rewraps_when_the_width_changes() {
        let mut t = transcript();
        t.push_user("one two three".into());

        assert_eq!(t.layout(10), 2);
        assert_eq!(t.layout(40), 1);
    }
}
