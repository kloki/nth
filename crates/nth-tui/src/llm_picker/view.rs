//! Draws the picker in the prompt's place: a header with its keys, then the
//! models, scrolled to the highlighted one.

use nth_protocol::{Failed, ModelInfo};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{LlmPicker, State};
use crate::theme::{BAR, dim, panel_row, panel_title, pick};

const TITLE: &str = "switch model";
const KEYS: &str = "↑↓ model · ←→ effort · enter · esc";

/// The picker's bar and title; see `theme::panel_title`.
const ACCENT: Color = Color::Magenta;

pub fn draw(frame: &mut Frame, area: Rect, picker: &LlmPicker) {
    let [header, list] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(Paragraph::new(panel_title(TITLE, ACCENT)), header);
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
        State::Ready {
            models,
            failed,
            selected,
        } => rows(picker, models, failed, *selected, usize::from(list.height)),
    };
    frame.render_widget(Paragraph::new(lines), list);
}

fn note(text: Span<'_>) -> Line<'_> {
    panel_row(ACCENT, [text])
}

/// One row per model: id, a ✓ on the one in use, name and limits, and on
/// the highlighted reasoning model the effort ←→ changes. The providers
/// that could not be listed follow, each a row saying why. The window
/// scrolls over all of them to keep the highlighted model in view.
fn rows<'a>(
    picker: &'a LlmPicker,
    models: &'a [ModelInfo],
    failed: &'a [Failed],
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
    let pick = pick();

    let mut lines: Vec<Line<'a>> = models
        .iter()
        .enumerate()
        .map(|(i, model)| {
            let here = i == selected;
            let (arrow, id) = if here {
                ("→ ", pick)
            } else {
                ("  ", Style::new().fg(Color::Blue))
            };
            let active = if model.id == picker.current {
                "✓"
            } else {
                " "
            };
            let name = model.name.as_deref().unwrap_or("");
            let mut spans = vec![
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
            panel_row(ACCENT, spans)
        })
        .collect();
    lines.extend(failed.iter().map(|failed| {
        panel_row(
            ACCENT,
            [Span::styled(
                format!("  ✗ {}: {}", failed.origin.name, failed.error),
                Style::new().fg(Color::Red),
            )],
        )
    }));
    let first = selected.saturating_sub(height.saturating_sub(1));
    lines.drain(..first);
    lines.truncate(height);
    lines
}
