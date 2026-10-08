//! Draws the settings panel in the prompt's place: a header with its keys,
//! then a row per setting with whether it is on.

use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Style},
    text::Span,
    widgets::Paragraph,
};

use super::{ChatSettings, Setting, SettingsPanel};
use crate::theme::{BAR, dim, panel_row, panel_title, pick};

const TITLE: &str = "settings";
const KEYS: &str = "↑↓ setting · enter toggle · esc";

/// The panel's bar and title; see `theme::panel_title`.
const ACCENT: Color = Color::Magenta;

pub fn draw(frame: &mut Frame, area: Rect, panel: &SettingsPanel, settings: ChatSettings) {
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

    let width = Setting::ALL
        .map(|s| s.name().len())
        .into_iter()
        .max()
        .unwrap_or(0);
    let lines: Vec<_> = Setting::ALL
        .into_iter()
        .map(|setting| {
            let (arrow, name) = if setting == panel.selected() {
                ("→ ", pick())
            } else {
                ("  ", Style::new())
            };
            let mark = if setting.get(settings) { "[x]" } else { "[ ]" };
            panel_row(
                ACCENT,
                [
                    Span::styled(arrow, pick()),
                    Span::styled(format!("{mark} {:width$}", setting.name()), name),
                    Span::styled(format!("  {}", setting.about()), dim()),
                ],
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), body);
}

#[cfg(test)]
mod tests {
    use ratatui::{Terminal, backend::TestBackend};

    use super::*;

    fn drawn(panel: &SettingsPanel, settings: ChatSettings) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(60, 3)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, frame.area(), panel, settings))
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..3)
            .map(|y| {
                (0..60)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn marks_what_is_on_and_the_highlighted_row() {
        let settings = ChatSettings {
            thinking: false,
            tool_output: true,
        };
        assert_eq!(
            drawn(&SettingsPanel::default(), settings),
            [
                "▎ settings                   ↑↓ setting · enter toggle · esc",
                "▎ → [ ] thinking     the model's reasoning under ∴",
                "▎   [x] tool output  what each tool call returned",
            ]
        );
    }
}
