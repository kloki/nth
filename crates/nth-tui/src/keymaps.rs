//! The TUI's keyboard reference, grouped by where each binding applies.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::{pager::Pager, theme};

#[derive(Debug, Default)]
pub struct Keymaps {
    pub pager: Pager,
}

impl Keymaps {
    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let lines = sections();
        self.pager.render(frame, area, lines);
    }
}

fn sections() -> Vec<Line<'static>> {
    let groups: &[(&str, &[(&str, &str)])] = &[
        (
            "content tabs · available from every input panel",
            &[
                ("Ctrl+T", "focus next open tab; wraps"),
                ("Ctrl+Shift+T", "focus previous open tab; wraps"),
                (
                    "Ctrl+1 … Ctrl+4",
                    "focus the first through fourth open tabs",
                ),
                (
                    "Ctrl+Q",
                    "close focused tab (chat cannot close; work stays while its plan exists)",
                ),
                (
                    "Ctrl+W",
                    "stop focused monitor or running subagent; close an idle subagent",
                ),
                ("PageUp · Ctrl+U", "scroll focused content upward"),
                ("PageDown · Ctrl+D", "scroll focused content downward"),
                (
                    "Ctrl+Home · Ctrl+End",
                    "jump focused content to top or bottom",
                ),
                ("Mouse wheel", "scroll the pane under the pointer"),
            ],
        ),
        (
            "prompt · normal input",
            &[
                (
                    "Ctrl+M",
                    "open model picker; use /models if terminal treats it as Enter",
                ),
                (
                    "Ctrl+P",
                    "cycle configured profiles; first restores a manually changed profile",
                ),
                (
                    "Ctrl+G",
                    "open external editor: prompt text, or plan copy for review feedback on Plan",
                ),
                (
                    "Ctrl+C",
                    "clear nonempty prompt; on empty prompt, quit (twice if agents/monitors run)",
                ),
                (
                    "! at cursor 0",
                    "enter shell mode; Esc or Backspace at start exits",
                ),
                ("Ctrl+N", "fill in selected completion; does not submit"),
            ],
        ),
        (
            "mouse",
            &[
                ("Left-click URL", "open link in browser"),
                ("Right-click", "copy content under pointer"),
            ],
        ),
    ];

    let mut lines = vec![Line::from(Span::styled(
        "KEYBOARD REFERENCE · scroll to browse · ctrl+q closes this tab",
        Style::new().fg(Color::Cyan),
    ))];
    let content_width: usize = 72;
    for (title, entries) in groups {
        let mut block = vec![Line::from(Span::styled(
            *title,
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        ))];
        let width = entries.iter().map(|(key, _)| key.len()).max().unwrap_or(0);
        for (key, description) in *entries {
            let label = format!("{key:width$}  ");
            let room = content_width.saturating_sub(label.len()).max(8);
            let mut remaining = *description;
            let mut first = true;
            while !remaining.is_empty() {
                let (part, rest) = take_words(remaining, room);
                block.push(Line::from(vec![
                    Span::styled(
                        if first {
                            label.clone()
                        } else {
                            " ".repeat(label.len())
                        },
                        Style::new().fg(Color::Yellow),
                    ),
                    Span::raw(part.to_string()),
                ]));
                remaining = rest;
                first = false;
            }
        }
        lines.extend(theme::sections(vec![block]));
        lines.push(Line::default());
    }
    lines
}

/// A word-aligned prefix no longer than `width`, and the remaining text.
fn take_words(text: &str, width: usize) -> (&str, &str) {
    if text.len() <= width {
        return (text, "");
    }
    let boundary = text[..width]
        .rfind(' ')
        .filter(|&at| at > 0)
        .unwrap_or(width);
    let (line, rest) = text.split_at(boundary);
    (line, rest.trim_start())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_sections_wrap_and_keep_the_later_groups() {
        for line in sections() {
            assert!(line.width() <= 96, "line is too wide: {line:?}");
        }
        assert!(
            sections()
                .iter()
                .any(|line| line.to_string().contains("mouse"))
        );
    }
}
