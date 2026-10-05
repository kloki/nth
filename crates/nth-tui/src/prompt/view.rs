//! Draws the prompt: the mode's bar down its left, its label on the top row,
//! and three rows of text under it, scrolled to the cursor.

use nth_protocol::Mode;
use ratatui::{
    Frame,
    layout::{Position, Rect},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::Prompt;
use crate::theme::{BAR_WIDTH, SHELL_COLOUR, dim, mode_colour, panel_row, panel_title};

const PLACEHOLDER: &str = "Ask anything.";
const SHELL_PLACEHOLDER: &str = "Run a command.";
/// The label while the prompt holds a command, in place of the mode's.
const SHELL_LABEL: &str = "cmd";
/// Right-aligned on the label row while a turn runs.
const CANCEL_HINT: &str = "esc to cancel";
/// The label row on top, then the text.
pub const ROWS: u16 = 1 + TEXT_ROWS;
const TEXT_ROWS: u16 = 3;

/// `spinner` replaces the mode's label while a turn runs, and the label
/// row says how to cancel. The text stays as it is: Enter queues it. A
/// command to run shows as `cmd` in yellow instead of the mode.
pub fn draw(frame: &mut Frame, area: Rect, prompt: &Prompt, mode: Mode, spinner: Option<&str>) {
    let (label, colour, placeholder) = match prompt.shell() {
        true => (SHELL_LABEL, SHELL_COLOUR, SHELL_PLACEHOLDER),
        false => (mode.label(), mode_colour(mode), PLACEHOLDER),
    };
    let label = spinner.unwrap_or(label);
    let wrapped = prompt.wrap(room(area));
    let top = scroll(wrapped.cursor_row);
    let text: Vec<Span> = if prompt.is_empty() {
        vec![Span::styled(placeholder, dim())]
    } else {
        wrapped.rows[top..]
            .iter()
            .take(usize::from(TEXT_ROWS))
            .map(|row| Span::raw(row.as_str()))
            .collect()
    };

    let mut lines = vec![panel_title(label, colour)];
    lines.extend((0..usize::from(TEXT_ROWS)).map(|i| panel_row(colour, text.get(i).cloned())));
    frame.render_widget(Paragraph::new(lines), area);
    if spinner.is_some() {
        let label_row = Rect { height: 1, ..area };
        frame.render_widget(
            Paragraph::new(Line::styled(CANCEL_HINT, dim()).right_aligned()),
            label_row,
        );
    }

    frame.set_cursor_position(position(area, prompt, prompt.cursor()));
}

/// Where byte `at` falls on the cursor's row on screen; a position on an
/// earlier row clamps to the row's start.
pub fn position(area: Rect, prompt: &Prompt, at: usize) -> Position {
    let wrapped = prompt.wrap(room(area));
    let back = prompt.text()[at..prompt.cursor()].chars().count();
    let col = wrapped.cursor_col.saturating_sub(back);
    let row = wrapped.cursor_row - scroll(wrapped.cursor_row);
    Position::new(
        area.x + BAR_WIDTH + u16::try_from(col).unwrap_or(0),
        area.y + 1 + u16::try_from(row).unwrap_or(0),
    )
}

fn room(area: Rect) -> usize {
    usize::from(area.width.saturating_sub(BAR_WIDTH))
}

/// The first text row shown: the top, until the cursor runs past the last
/// row, then whatever keeps the cursor on the bottom one.
fn scroll(cursor_row: usize) -> usize {
    cursor_row.saturating_sub(usize::from(TEXT_ROWS) - 1)
}
