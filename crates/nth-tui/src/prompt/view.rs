//! Draws the prompt: the mode's bar down its left, its label on the top row,
//! and three rows of text under it, scrolled to the cursor.

use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::Style,
    text::Span,
    widgets::Paragraph,
};

use super::{Mode, Prompt};
use crate::theme::{BAR_WIDTH, dim, panel_row, panel_title};

const PLACEHOLDER: &str = "Ask anything.";
/// The label row on top, then the text.
pub const ROWS: u16 = 1 + TEXT_ROWS;
const TEXT_ROWS: u16 = 3;

/// `spinner` replaces the mode's label while a turn runs; the text is
/// dimmed then too, since Enter won't submit.
pub fn draw(frame: &mut Frame, area: Rect, prompt: &Prompt, mode: Mode, spinner: Option<&str>) {
    let label = spinner.unwrap_or(mode.label());
    let wrapped = prompt.wrap(room(area));
    let top = scroll(wrapped.cursor_row);
    let text_style = if spinner.is_some() {
        dim()
    } else {
        Style::new()
    };
    let text: Vec<Span> = if prompt.is_empty() {
        vec![Span::styled(PLACEHOLDER, dim())]
    } else {
        wrapped.rows[top..]
            .iter()
            .take(usize::from(TEXT_ROWS))
            .map(|row| Span::styled(row.as_str(), text_style))
            .collect()
    };

    let mut lines = vec![panel_title(label, mode.colour())];
    lines.extend(
        (0..usize::from(TEXT_ROWS)).map(|i| panel_row(mode.colour(), text.get(i).cloned())),
    );
    frame.render_widget(Paragraph::new(lines), area);

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
