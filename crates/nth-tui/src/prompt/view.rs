//! Draws the prompt bar: as many rows as it is given, scrolled to the cursor.

use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::Prompt;
use crate::theme::{BAR, BAR_WIDTH, dim};

const PLACEHOLDER: &str = "Ask anything.";

/// `busy` dims the bar while a turn runs, since Enter won't submit.
pub fn draw(frame: &mut Frame, area: Rect, prompt: &Prompt, busy: bool) {
    let room = usize::from(area.width.saturating_sub(BAR_WIDTH));
    let wrapped = prompt.wrap(room);
    let rows = usize::from(area.height).max(1);
    // Scroll inside the box just enough to keep the cursor row visible.
    let first = wrapped.cursor_row.saturating_sub(rows - 1);

    let bar = if prompt.is_empty() || busy {
        dim()
    } else {
        Style::new().fg(Color::Blue)
    };
    let lines: Vec<Line> = (first..first + rows)
        .map(|i| {
            let text = match wrapped.rows.get(i) {
                _ if prompt.is_empty() && i == 0 => Span::styled(PLACEHOLDER, dim()),
                Some(row) => Span::raw(row.as_str()),
                None => Span::raw(""),
            };
            Line::from(vec![Span::styled(BAR, bar), text])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);

    let row = u16::try_from(wrapped.cursor_row - first).unwrap_or(0);
    let col = u16::try_from(wrapped.cursor_col).unwrap_or(0);
    frame.set_cursor_position(Position::new(area.x + BAR_WIDTH + col, area.y + row));
}
