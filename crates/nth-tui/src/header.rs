//! The header above the content panel: one line naming the app and its
//! version, on the right.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::theme;

/// Always this tall.
pub const ROWS: u16 = 1;

pub fn draw(frame: &mut Frame, area: Rect) {
    let line = Line::from(vec![
        Span::styled(
            "nth",
            Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
        ),
        Span::styled(concat!(" ", env!("CARGO_PKG_VERSION")), theme::dim()),
    ])
    .right_aligned();
    frame.render_widget(Paragraph::new(line), area);
}
