//! Draws the picker in the prompt's place: a header with its keys, then the
//! models, grouped under their providers when there are several, scrolled
//! to the highlighted one.

use std::collections::BTreeSet;

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
/// the highlighted reasoning model the effort ←→ changes. With models from
/// several providers, each provider's rows sit under its name. The
/// providers that could not be listed follow, each a row saying why. The
/// window scrolls over all of them to keep the highlighted model in view,
/// with its provider's name when it is the first under it.
fn rows<'a>(
    picker: &'a LlmPicker,
    models: &'a [ModelInfo],
    failed: &'a [Failed],
    selected: usize,
    height: usize,
) -> Vec<Line<'a>> {
    let origins: BTreeSet<&str> = models
        .iter()
        .filter_map(|m| m.origin.as_ref().map(|o| o.id.as_str()))
        .collect();
    let grouped = origins.len() > 1;
    let id_width = models
        .iter()
        .map(|m| m.wire_id().chars().count())
        .max()
        .unwrap_or(0);
    let name_width = models
        .iter()
        .map(|m| m.name.as_deref().unwrap_or("").chars().count())
        .max()
        .unwrap_or(0);
    let pick = pick();

    // Each line with whether it is a provider's name, and where the
    // highlighted model's line is.
    let mut lines: Vec<(Line<'a>, bool)> = Vec::new();
    let mut selected_line = 0;
    let mut shown: Option<&str> = None;
    for (i, model) in models.iter().enumerate() {
        if grouped {
            let origin = model.origin.as_ref().map(|o| o.id.as_str());
            if origin != shown {
                let name = model.origin.as_ref().map_or("", |o| o.name.as_str());
                lines.push((panel_row(ACCENT, [Span::styled(name, dim())]), true));
                shown = origin;
            }
        }
        if i == selected {
            selected_line = lines.len();
        }
        lines.push((
            {
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
                    Span::styled(format!("{:<id_width$} ", model.wire_id()), id),
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
            },
            false,
        ));
    }
    lines.extend(failed.iter().map(|failed| {
        (
            panel_row(
                ACCENT,
                [Span::styled(
                    format!("  ✗ {}: {}", failed.origin.name, failed.error),
                    Style::new().fg(Color::Red),
                )],
            ),
            false,
        )
    }));
    let mut first = (selected_line + 1).saturating_sub(height);
    let under_name = selected_line > 0 && lines[selected_line - 1].1;
    if under_name && first == selected_line && height > 1 {
        first -= 1;
    }
    lines
        .into_iter()
        .skip(first)
        .take(height)
        .map(|(line, _)| line)
        .collect()
}

#[cfg(test)]
mod tests {
    use nth_protocol::{Effort, Listing, Origin};

    use super::*;

    fn model(origin: &str, id: &str) -> ModelInfo {
        ModelInfo {
            id: format!("{origin}/{id}"),
            name: None,
            context: None,
            output: None,
            reasoning: false,
            origin: Some(Origin {
                id: origin.into(),
                name: origin.to_uppercase(),
            }),
        }
    }

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    fn picker(models: Vec<ModelInfo>, current: &str) -> LlmPicker {
        let mut picker = LlmPicker::new(current, Effort::Default);
        picker.load(Ok(Listing::from(models)));
        picker
    }

    #[test]
    fn models_from_several_providers_sit_under_their_names() {
        let models = vec![model("lyceum", "glm"), model("opencode", "kimi")];
        let picker = picker(models.clone(), "opencode/kimi");

        let lines: Vec<_> = rows(&picker, &models, &[], 1, 8).iter().map(text).collect();

        assert_eq!(lines, ["▎ LYCEUM", "▎   glm", "▎ OPENCODE", "▎ → kimi ✓"]);
    }

    #[test]
    fn one_provider_needs_no_name() {
        let models = vec![model("lyceum", "glm"), model("lyceum", "kimi")];
        let picker = picker(models.clone(), "x");

        let lines: Vec<_> = rows(&picker, &models, &[], 0, 8).iter().map(text).collect();

        assert_eq!(lines, ["▎ → glm", "▎   kimi"], "ids without the prefix");
    }

    #[test]
    fn the_window_keeps_the_highlighted_model_and_its_name_in_view() {
        let models = vec![model("lyceum", "glm"), model("opencode", "kimi")];
        let picker = picker(models.clone(), "x");

        let lines: Vec<_> = rows(&picker, &models, &[], 1, 3).iter().map(text).collect();
        assert_eq!(lines, ["▎   glm", "▎ OPENCODE", "▎ → kimi"]);

        let lines: Vec<_> = rows(&picker, &models, &[], 1, 2).iter().map(text).collect();
        assert_eq!(lines, ["▎ OPENCODE", "▎ → kimi"], "the name comes along");

        let lines: Vec<_> = rows(&picker, &models, &[], 1, 1).iter().map(text).collect();
        assert_eq!(lines, ["▎ → kimi"], "but never instead of the model");
    }
}
