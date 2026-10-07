//! Draws the picker in the prompt's place: a header with its keys, then the
//! models, the most used first, scrolled to the highlighted one.

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
/// several providers, ids keep their `provider/` prefix. The providers that
/// could not be listed follow, each a row saying why. The window scrolls
/// over all of them to keep the highlighted model in view.
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
    let shown_id = |m: &'a ModelInfo| {
        if origins.len() > 1 {
            m.id.as_str()
        } else {
            m.wire_id()
        }
    };
    let id_width = models
        .iter()
        .map(|m| shown_id(m).chars().count())
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
                Span::styled(format!("{:<id_width$} ", shown_id(model)), id),
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
    let first = (selected + 1).saturating_sub(height);
    lines.into_iter().skip(first).take(height).collect()
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
    fn models_from_several_providers_keep_their_prefix() {
        let models = vec![model("opencode", "kimi"), model("lyceum", "glm")];
        let picker = picker(models.clone(), "opencode/kimi");

        let lines: Vec<_> = rows(&picker, &models, &[], 1, 8).iter().map(text).collect();

        assert_eq!(lines, ["▎   opencode/kimi ✓", "▎ → lyceum/glm"]);
    }

    #[test]
    fn one_provider_needs_no_prefix() {
        let models = vec![model("lyceum", "glm"), model("lyceum", "kimi")];
        let picker = picker(models.clone(), "x");

        let lines: Vec<_> = rows(&picker, &models, &[], 0, 8).iter().map(text).collect();

        assert_eq!(lines, ["▎ → glm", "▎   kimi"]);
    }

    #[test]
    fn the_window_keeps_the_highlighted_model_in_view() {
        let models = vec![
            model("lyceum", "glm"),
            model("lyceum", "kimi"),
            model("lyceum", "qwen"),
        ];
        let picker = picker(models.clone(), "x");

        let lines: Vec<_> = rows(&picker, &models, &[], 2, 2).iter().map(text).collect();
        assert_eq!(lines, ["▎   kimi", "▎ → qwen"]);
    }
}
