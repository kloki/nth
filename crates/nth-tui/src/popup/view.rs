//! Draws the popup: a grey box sized to its rows, the selected one in bold
//! purple, sitting just above `anchor` and over whatever is there.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

/// `area` bounds the popup; `anchor` is the prompt it sits above. Each row
/// is a label, drawn in blue, and a detail after it.
pub fn draw(frame: &mut Frame, area: Rect, anchor: Rect, rows: &[(String, &str)], selected: usize) {
    let content = rows
        .iter()
        .map(|(label, detail)| label.chars().count() + detail.chars().count())
        .max()
        .unwrap_or(0);
    // One column of padding either side.
    let width = u16::try_from(content + 2)
        .unwrap_or(u16::MAX)
        .min(area.right().saturating_sub(anchor.x));
    let height = u16::try_from(rows.len())
        .unwrap_or(u16::MAX)
        .min(anchor.y.saturating_sub(area.y));
    let popup = Rect {
        x: anchor.x,
        y: anchor.y - height,
        width,
        height,
    };

    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .map(|(i, (label, detail))| {
            let pad = content - label.chars().count();
            let (row, label_style) = if i == selected {
                let row = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
                (row, row)
            } else {
                (Style::new(), Style::new().fg(Color::Blue))
            };
            Line::from(vec![
                Span::raw(" "),
                Span::styled(label.as_str(), label_style),
                Span::raw(format!("{detail:<pad$} ")),
            ])
            .style(row)
        })
        .collect();
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines).style(Style::new().bg(Color::DarkGray).fg(Color::White)),
        popup,
    );
}
