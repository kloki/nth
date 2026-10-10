//! Draws the picker in the prompt's place: a header with its keys, the
//! query, then the models matching it, scrolled to the highlighted one.

use nth_icons::icons;
use nth_protocol::{Failed, ModelInfo};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{LlmPicker, State, haystack, shown_id};
use crate::{
    fuzzy::{self, Filter, query_row},
    theme::{BAR, dim, panel_row, panel_title, pick},
};

const TITLE: &str = "switch model";
const KEYS: &str = "type to filter · ↑↓ model · ←→ effort · enter · esc";

/// The picker's bar and title; see `theme::panel_title`.
const ACCENT: Color = Color::Magenta;

pub fn draw(frame: &mut Frame, area: Rect, picker: &LlmPicker) {
    let [header, body] = Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
    frame.render_widget(Paragraph::new(panel_title(TITLE, ACCENT)), header);
    // Dropped rather than drawn over the title when the row is too narrow.
    let room = usize::from(header.width);
    if room > BAR.chars().count() + TITLE.len() + KEYS.chars().count() + 2 {
        frame.render_widget(
            Paragraph::new(Span::styled(KEYS, dim())).alignment(Alignment::Right),
            header,
        );
    }

    let (lines, list) = match &picker.state {
        State::Loading => (vec![note(Span::styled("loading models…", dim()))], body),
        State::Failed(error) => (
            vec![note(Span::styled(
                format!("{} {error}", icons().fail),
                Style::new().fg(Color::Red),
            ))],
            body,
        ),
        State::Ready {
            models,
            failed,
            prefixed,
            filter,
            shown,
            selected,
        } => {
            // The query row only once there is a list to filter.
            let [query, list] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(body);
            query_row(
                frame,
                query,
                ACCENT,
                filter.query(),
                shown.len(),
                models.len(),
            );
            let lines = rows(
                picker,
                Listed {
                    models,
                    failed,
                    prefixed: *prefixed,
                    filter,
                    shown,
                },
                *selected,
                usize::from(list.height),
            );
            (lines, list)
        }
    };
    frame.render_widget(Paragraph::new(lines), list);
}

fn note(text: Span<'_>) -> Line<'_> {
    panel_row(ACCENT, [text])
}

/// The ready list, as `rows` draws it.
struct Listed<'a> {
    models: &'a [ModelInfo],
    failed: &'a [Failed],
    prefixed: bool,
    filter: &'a Filter,
    shown: &'a [usize],
}

/// One row per model matching the query: id, a ✓ on the one in use, name
/// and limits, and on the highlighted model that takes one the effort ←→
/// changes, with the characters the query matched marked. The providers
/// that could not be listed follow, each a row saying why. The window
/// scrolls over all of them to keep the highlighted model in view.
fn rows<'a>(
    picker: &'a LlmPicker,
    listed: Listed<'a>,
    selected: usize,
    height: usize,
) -> Vec<Line<'a>> {
    let Listed {
        models,
        failed,
        prefixed,
        filter,
        shown,
    } = listed;
    // Widths over every model, so columns stay put while filtering.
    let id_width = models
        .iter()
        .map(|m| shown_id(m, prefixed).chars().count())
        .max()
        .unwrap_or(0);
    let name_width = models
        .iter()
        .map(|m| m.name.as_deref().unwrap_or("").chars().count())
        .max()
        .unwrap_or(0);
    let pick = pick();

    let mut lines: Vec<Line<'a>> = shown
        .iter()
        .enumerate()
        .map(|(row, &i)| {
            let model = &models[i];
            let here = row == selected;
            let (arrow, id_style) = if here {
                (format!("{} ", icons().pick), pick)
            } else {
                ("  ".to_string(), Style::new().fg(Color::Blue))
            };
            let active = if model.id == picker.current {
                icons().ok
            } else {
                " "
            };
            let id = shown_id(model, prefixed);
            let name = model.name.as_deref().unwrap_or("");
            let hits = filter.indices(&haystack(model, prefixed));
            let mut spans = vec![Span::styled(arrow, pick)];
            spans.extend(fuzzy::highlight(id, &hits, 0, id_style, hit(id_style)));
            spans.push(Span::raw(" ".repeat(id_width - id.chars().count() + 1)));
            spans.push(Span::styled(active, Style::new().fg(Color::Green)));
            spans.push(Span::raw(" "));
            let offset = id.chars().count() + 1;
            spans.extend(fuzzy::highlight(name, &hits, offset, dim(), hit(dim())));
            spans.push(Span::styled(
                format!(
                    "{}  {}",
                    " ".repeat(name_width - name.chars().count()),
                    model.limits()
                ),
                dim(),
            ));
            if let Some(effort) = here.then(|| picker.shown_effort()).flatten() {
                spans.push(Span::styled(
                    format!(
                        "  {} {} {}",
                        icons().effort_less,
                        effort.name(),
                        icons().effort_more
                    ),
                    pick,
                ));
            }
            panel_row(ACCENT, spans)
        })
        .collect();
    if shown.is_empty() {
        lines.push(note(Span::styled("  no matches", dim())));
    }
    lines.extend(failed.iter().map(|failed| {
        panel_row(
            ACCENT,
            [Span::styled(
                format!(
                    "  {} {}: {}",
                    icons().fail,
                    failed.origin.name,
                    failed.error
                ),
                Style::new().fg(Color::Red),
            )],
        )
    }));
    let first = (selected + 1).saturating_sub(height);
    lines.into_iter().skip(first).take(height).collect()
}

/// A matched character: `style`, underlined and bold.
fn hit(style: Style) -> Style {
    style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
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
            efforts: Vec::new(),
            origin: Some(Origin {
                id: origin.into(),
                name: origin.to_uppercase(),
            }),
            cost: None,
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

    /// The picker's rows as text, with `selected` highlighted, `height` at
    /// most.
    fn drawn(picker: &LlmPicker, selected: usize, height: usize) -> Vec<String> {
        lines(picker, selected, height).iter().map(text).collect()
    }

    fn lines(picker: &LlmPicker, selected: usize, height: usize) -> Vec<Line<'_>> {
        let State::Ready {
            models,
            failed,
            prefixed,
            filter,
            shown,
            ..
        } = &picker.state
        else {
            panic!("not ready");
        };
        let listed = Listed {
            models,
            failed,
            prefixed: *prefixed,
            filter,
            shown,
        };
        rows(picker, listed, selected, height)
    }

    #[test]
    fn models_from_several_providers_keep_their_prefix() {
        let models = vec![model("opencode", "kimi"), model("lyceum", "glm")];
        let picker = picker(models, "opencode/kimi");

        assert_eq!(
            drawn(&picker, 1, 8),
            ["▎   opencode/kimi ✓", "▎ → lyceum/glm"]
        );
    }

    #[test]
    fn one_provider_needs_no_prefix() {
        let models = vec![model("lyceum", "glm"), model("lyceum", "kimi")];
        let picker = picker(models, "x");

        assert_eq!(drawn(&picker, 0, 8), ["▎ → glm", "▎   kimi"]);
    }

    #[test]
    fn the_window_keeps_the_highlighted_model_in_view() {
        let models = vec![
            model("lyceum", "glm"),
            model("lyceum", "kimi"),
            model("lyceum", "qwen"),
        ];
        let picker = picker(models, "x");

        assert_eq!(drawn(&picker, 2, 2), ["▎   kimi", "▎ → qwen"]);
    }

    #[test]
    fn the_query_narrows_the_rows_and_marks_its_matches() {
        let models = vec![model("lyceum", "glm"), model("lyceum", "kimi")];
        let mut picker = picker(models, "x");
        picker.insert('k');
        picker.insert('m');

        assert_eq!(drawn(&picker, 0, 8), ["▎ → kimi"]);
        let marked: String = lines(&picker, 0, 8)[0]
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(marked, "km");

        picker.insert('z');
        assert_eq!(drawn(&picker, 0, 8), ["▎   no matches"]);
    }
}
