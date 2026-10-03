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
}

/// An entry whose line changes without an event, so it is redrawn each frame.
fn is_live(entry: &Entry) -> bool {
    matches!(entry, Entry::Reasoning { took: None, .. })
}

/// One-line rows that stack with no gap between them.
fn is_compact(entry: &Entry) -> bool {
    matches!(
        entry,
        Entry::Reasoning { .. }
            | Entry::Tool { .. }
            | Entry::TurnDone { .. }
            | Entry::Interrupted { .. }
    )
}

fn needs_gap(previous: Option<&Entry>, entry: &Entry) -> bool {
    match previous {
        None => false,
        // A turn's footer stands apart from the turn it closes.
        Some(_) if matches!(entry, Entry::TurnDone { .. } | Entry::Interrupted { .. }) => true,
        Some(previous) => !(is_compact(previous) && is_compact(entry)),
    }
}

fn render(entry: &Entry, cwd: &std::path::Path, width: u16) -> Vec<Line<'static>> {
    let dim = dim();
    match entry {
        Entry::User(text) => barred(text, width, Style::new().fg(Color::Green), Style::new()),
        Entry::Answer(text) => barred(text, width, Style::new().fg(Color::DarkGray), Style::new()),
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
                // Closes the turn as `∴` opens its thinking; ✓ is left to
                // tool calls, where it means success.
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
                "  ▸ read   src/a.rs",
                "",
                "  ∎ glm · 1 tool call · 2.0s",
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
