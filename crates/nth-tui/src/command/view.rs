//! Draws the completion popup: a grey box sized to its rows, the selected
//! one in bold purple, sitting just above `anchor` and over whatever is there.

use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

use super::Completion;

/// Longest command name plus its leading `/`, so descriptions line up.
const NAME_WIDTH: usize = 8;

/// `area` bounds the popup; `anchor` is the prompt it sits above.
pub fn draw(frame: &mut Frame, area: Rect, anchor: Rect, completion: &Completion) {
    let rows: Vec<(String, &str)> = completion
        .matches
        .iter()
        .map(|c| (format!("/{:<NAME_WIDTH$}", c.name()), c.about()))
        .collect();
    let content = rows
        .iter()
        .map(|(name, about)| name.chars().count() + about.chars().count())
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

    let selected = completion.selected;
    let lines: Vec<Line> = rows
        .into_iter()
        .enumerate()
        .map(|(i, (name, about))| {
            let pad = content - name.chars().count();
            let about = format!("{about:<pad$}");
            let (row, name_style) = if i == selected {
                let row = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
                (row, row)
            } else {
                (Style::new(), Style::new().fg(Color::Blue))
            };
            Line::from(vec![
                Span::raw(" "),
                Span::styled(name, name_style),
                Span::raw(about),
                Span::raw(" "),
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
