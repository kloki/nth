//! Draws the prompt: a lighter block with the mode's bar down its left, and
//! one row of text between padding rows, scrolled to the cursor.

use ratatui::{
    Frame,
    layout::{Position, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{LABEL_WIDTH, Mode, Prompt};
use crate::theme::{BAR, BAR_WIDTH, dim};

const PLACEHOLDER: &str = "Ask anything.";
/// Between the label and the text.
const ARROW: &str = "  > ";

/// The text starts this far in from the bar's column.
const PREFIX_WIDTH: u16 = BAR_WIDTH + LABEL_WIDTH as u16 + ARROW.len() as u16;

/// `spinner` replaces the mode's label while a turn runs; the text is
/// dimmed then too, since Enter won't submit.
pub fn draw(frame: &mut Frame, area: Rect, prompt: &Prompt, mode: Mode, spinner: Option<&str>) {
    let mode_style = Style::new().fg(mode.colour());
    let label = spinner.unwrap_or(mode.label());
    let room = usize::from(area.width.saturating_sub(PREFIX_WIDTH));
    let wrapped = prompt.wrap(room);
    let text = match wrapped.rows.get(wrapped.cursor_row) {
        _ if prompt.is_empty() => Span::styled(PLACEHOLDER, dim()),
        Some(row) if spinner.is_some() => Span::styled(row.clone(), dim()),
        Some(row) => Span::raw(row.clone()),
        None => Span::raw(""),
    };
    let bar = || Line::from(Span::styled(BAR, mode_style));
    let lines = vec![
        bar(),
        Line::from(vec![
            Span::styled(BAR, mode_style),
            Span::styled(format!("{label:<LABEL_WIDTH$}"), mode_style),
            Span::raw(ARROW),
            text,
        ]),
        bar(),
    ];
    frame.render_widget(
        Paragraph::new(lines).style(Style::new().bg(Color::DarkGray)),
        area,
    );

    let col = u16::try_from(wrapped.cursor_col).unwrap_or(0);
    frame.set_cursor_position(Position::new(area.x + PREFIX_WIDTH + col, area.y + 1));
}
