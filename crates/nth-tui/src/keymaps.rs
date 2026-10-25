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
                ("Enter", "submit prompt or execute a slash command"),
                (
                    "Ctrl+J · Ctrl+Enter · Shift+Enter",
                    "insert a newline (modified Enter depends on terminal)",
                ),
                ("Left · Right", "move cursor one character"),
                ("Home · End", "move to start or end of current line"),
                (
                    "Backspace · Delete",
                    "delete character before or after cursor",
                ),
                (
                    "Up · Down",
                    "recall older or newer sent prompts; preserves the draft",
                ),
                (
                    "Tab · Shift+Tab",
                    "switch plan/act; leave a primary agent for the mode",
                ),
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
                    "Esc",
                    "cancel turn; on a subagent tab, interrupt that subagent",
                ),
                (
                    "Ctrl+C",
                    "clear nonempty prompt; on empty prompt, quit (twice if agents/monitors run)",
                ),
                (
                    "! at cursor 0",
                    "enter shell mode; Esc or Backspace at start exits",
                ),
                (
                    "Enter mid-turn",
                    "send/queue a prompt; on subagent tab, message it (shell commands run in parent chat)",
                ),
            ],
        ),
        (
            "completion · slash commands, skills, @mentions, paths",
            &[
                ("Up · Down", "select previous or next completion"),
                ("Ctrl+N", "fill in selected completion; does not submit"),
                (
                    "Enter",
                    "accept; built-in commands run, mentions fill in, exact skill runs",
                ),
                ("Esc", "dismiss popup without cancelling the turn"),
                (
                    "Typing · Backspace · Delete",
                    "edit prompt and refresh matches",
                ),
                ("Tab", "switch plan/act, not completion acceptance"),
            ],
        ),
        (
            "model picker · /models or Ctrl+M",
            &[
                ("Up · Down", "select model"),
                ("Type · Backspace", "filter model list"),
                ("Left · Right", "decrease or increase reasoning effort"),
                (
                    "Tab · Shift+Tab",
                    "cycle target: plan, act, subagent, clerk (and active agent)",
                ),
                ("Enter", "apply model/effort; picker stays open"),
                ("Ctrl+C", "clear query; close when query is empty"),
                ("Esc · Ctrl+M", "close picker"),
            ],
        ),
        (
            "session picker · /resume",
            &[
                ("Up · Down · Type · Backspace", "select and filter sessions"),
                ("Enter", "resume selected session"),
                ("Ctrl+C", "clear query; close when query is empty"),
                ("Esc · Ctrl+M", "close picker"),
            ],
        ),
        (
            "agent picker · /agents",
            &[
                (
                    "Up · Down · Type · Backspace",
                    "select and filter modes and agents",
                ),
                ("Enter", "choose agent for later turns"),
                ("Ctrl+C", "clear query; close when query is empty"),
                ("Esc", "close picker (Ctrl+M is ignored here)"),
            ],
        ),
        (
            "curios picker · /curios",
            &[
                (
                    "Up · Down · Type · Backspace",
                    "select and filter review documents",
                ),
                ("Enter", "open selected Curio in browser"),
                ("Ctrl+Y", "copy selected Curio URL, when hosted"),
                (
                    "Esc · Ctrl+C · Ctrl+M",
                    "close picker (Ctrl+C does not clear filter first)",
                ),
            ],
        ),
        (
            "settings · /settings",
            &[
                ("Up · Down", "select setting"),
                ("Enter · Space", "toggle setting for this run"),
                ("Esc · Ctrl+C · Ctrl+M", "close settings"),
            ],
        ),
        (
            "provider connection · /connect",
            &[
                ("Up · Down", "select provider before login"),
                ("Enter", "start browser login"),
                ("c", "copy login URL while waiting"),
                ("Esc · Ctrl+C", "cancel login and close panel"),
            ],
        ),
        (
            "questions · asked by a tool",
            &[
                ("Up · Down", "select answer option or custom-answer row"),
                (
                    "Number keys",
                    "select option; multi-choice toggles; last number picks custom answer",
                ),
                (
                    "Space",
                    "toggle multi-choice option; in custom field inserts a space",
                ),
                ("Tab · Shift+Tab", "move between questions and submit tab"),
                (
                    "Left · Right",
                    "change question tab, or move cursor in custom field",
                ),
                ("Enter", "answer and advance; on submit tab, send answers"),
                (
                    "Type · Backspace · Delete · Home · End",
                    "enter/edit highlighted custom answer",
                ),
                (
                    "Esc · Ctrl+C",
                    "decline question batch; does not cancel turn",
                ),
            ],
        ),
        (
            "terminal notes",
            &[
                (
                    "Ctrl+M · modified Enter",
                    "may be indistinguishable from Enter on some terminals",
                ),
                (
                    "Ctrl+Shift+T · Ctrl+digits",
                    "require terminal escape-code disambiguation",
                ),
                (
                    "Ctrl+J",
                    "portable newline in prompt; question fields do not accept newlines",
                ),
                (
                    "Tab · Enter · Esc · Ctrl+C",
                    "meaning depends on the active input panel and focused tab",
                ),
                (
                    "Ctrl+Q · Ctrl+W",
                    "Ctrl+Q closes a tab; Ctrl+W stops monitor/subagent",
                ),
                (
                    "Home · End · Ctrl+Home · Ctrl+End",
                    "line cursor movement vs content top/bottom scrolling",
                ),
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
                .any(|line| line.to_string().contains("terminal notes"))
        );
    }
}
