//! Draws the picker in the prompt's place: a header with its keys, then the
//! models, scrolled to the highlighted one.

use nth_protocol::ModelInfo;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{Picker, State};
use crate::theme::{BAR, dim};

const TITLE: &str = "switch model";
const KEYS: &str = "↑↓ model · ←→ effort · enter · esc";

pub fn draw(frame: &mut Frame, area: Rect, picker: &Picker) {
    let [header, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    let bar = Style::new().fg(Color::Blue);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(BAR, bar),
            Span::styled(TITLE, Style::new().add_modifier(Modifier::BOLD)),
        ])),
        header,
    );
    // Dropped rather than drawn over the title when the row is too narrow.
    let room = usize::from(header.width);
    if room > BAR.chars().count() + TITLE.len() + KEYS.chars().count() + 2 {
        frame.render_widget(
            Paragraph::new(Span::styled(KEYS, dim())).alignment(Alignment::Right),
            header,
        );
    }

    let lines = match &picker.state {
        State::Loading => vec![note(Span::styled("loading models…", dim()))],
        State::Failed(error) => vec![note(Span::styled(
            format!("✗ {error}"),
            Style::new().fg(Color::Red),
        ))],
        State::Ready { models, selected } => {
            rows(picker, models, *selected, usize::from(list.height))
        }
    };
    frame.render_widget(Paragraph::new(lines), list);
}

fn note(text: Span<'_>) -> Line<'_> {
    Line::from(vec![Span::styled(BAR, dim()), text])
}

/// One row per model: id, a ✓ on the one in use, name and limits, and on
/// the highlighted reasoning model the effort ←→ changes.
fn rows<'a>(
    picker: &'a Picker,
    models: &'a [ModelInfo],
    selected: usize,
    height: usize,
) -> Vec<Line<'a>> {
    let id_width = models
        .iter()
        .map(|m| m.id.chars().count())
        .max()
        .unwrap_or(0);
    let name_width = models
        .iter()
        .map(|m| m.name.as_deref().unwrap_or("").chars().count())
        .max()
        .unwrap_or(0);
    let first = selected.saturating_sub(height.saturating_sub(1));
    let pick = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);

    models
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(i, model)| {
            let here = i == selected;
            let (bar, arrow, id) = if here {
                (pick, "→ ", pick)
            } else {
                (dim(), "  ", Style::new().fg(Color::Blue))
            };
            let active = if model.id == picker.current {
                "✓"
            } else {
                " "
            };
            let name = model.name.as_deref().unwrap_or("");
            let mut spans = vec![
                Span::styled(BAR, bar),
                Span::styled(arrow, pick),
                Span::styled(format!("{:<id_width$} ", model.id), id),
                Span::styled(active, Style::new().fg(Color::Green)),
                Span::styled(format!(" {name:<name_width$}  {}", model.limits()), dim()),
            ];
            if here && model.reasoning {
                spans.push(Span::styled(
                    format!("  ◂ {} ▸", picker.effort.name()),
                    pick,
                ));
            }
            Line::from(spans)
        })
        .collect()
}
