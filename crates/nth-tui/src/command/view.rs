//! Draws the completion list: one row per matching command, the selected
//! one highlighted, sitting on the bottom rows of `area`.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

use super::Completion;
use crate::theme::{BAR, dim};

pub fn draw(frame: &mut Frame, area: Rect, completion: &Completion) {
    let height = u16::try_from(completion.len())
        .unwrap_or(u16::MAX)
        .min(area.height);
    let area = Rect {
        y: area.bottom() - height,
        height,
        ..area
    };
    let selected = completion.selected();
    let lines: Vec<Line> = completion
        .matches
        .iter()
        .map(|&command| {
            let name = Style::new().fg(Color::Blue);
            let (bar, name) = if command == selected {
                (name, name.add_modifier(Modifier::BOLD))
            } else {
                (dim(), dim())
            };
            Line::from(vec![
                Span::styled(BAR, bar),
                Span::styled(format!("/{:<8}", command.name()), name),
                Span::styled(command.about(), dim()),
            ])
        })
        .collect();
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines), area);
}
