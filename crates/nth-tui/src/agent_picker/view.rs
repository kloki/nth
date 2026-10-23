//! Draws the picker in the prompt's place: a header with its keys, the
//! query, then the modes and agents matching it, each with what it does.

use nth_icons::icons;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{AgentPicker, Choice, haystack};
use crate::{
    fuzzy::{self, query_row},
    theme::{AGENT_COLOUR, BAR, dim, mode_colour, panel_row, panel_title, pick},
};

const TITLE: &str = "run as";
const KEYS: &str = "type to filter · ↑↓ agent · enter · esc";
/// The picker's bar and title; see `theme::panel_title`.
const ACCENT: Color = AGENT_COLOUR;

pub fn draw(frame: &mut Frame, area: Rect, picker: &AgentPicker) {
    let [header, query, list] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(panel_title(TITLE, ACCENT)), header);
    // Dropped rather than drawn over the title when the row is too narrow.
    let room = usize::from(header.width);
    if room > BAR.chars().count() + TITLE.len() + KEYS.chars().count() + 2 {
        frame.render_widget(
            Paragraph::new(Span::styled(KEYS, dim())).alignment(Alignment::Right),
            header,
        );
    }
    query_row(
        frame,
        query,
        ACCENT,
        picker.filter.query(),
        picker.shown.len(),
        picker.items.len(),
    );
    let lines = rows(picker, usize::from(list.width), usize::from(list.height));
    frame.render_widget(Paragraph::new(lines), list);
}

/// One row per item matching the query: a ✓ on what runs now, its name in
/// its colour, padded to the longest, then what it does, with the
/// characters the query matched marked.
fn rows(picker: &AgentPicker, width: usize, height: usize) -> Vec<Line<'static>> {
    if picker.shown.is_empty() {
        return vec![panel_row(ACCENT, [Span::styled("  no matches", dim())])];
    }
    let name_width = picker
        .items
        .iter()
        .map(|item| item.name().chars().count())
        .max()
        .unwrap_or(0);
    let first = picker.selected.saturating_sub(height.saturating_sub(1));
    picker
        .shown
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(row, &i)| {
            let item = &picker.items[i];
            let colour = match &item.choice {
                Choice::Mode(mode) => mode_colour(*mode),
                Choice::Agent(_) => AGENT_COLOUR,
            };
            let (arrow, name_style) = match row == picker.selected {
                true => (format!("{} ", icons().pick), pick()),
                false => ("  ".to_string(), Style::new().fg(colour)),
            };
            let mark = match item.choice == picker.current {
                true => icons().ok,
                false => " ",
            };
            let name = item.name();
            let hits = picker.filter.indices(&haystack(item));
            let mut spans = vec![
                Span::styled(arrow, pick()),
                Span::styled(mark, Style::new().fg(Color::Green)),
                Span::raw(" "),
            ];
            spans.extend(fuzzy::highlight(
                name,
                &hits,
                0,
                name_style,
                hit(name_style),
            ));
            spans.push(Span::raw(" ".repeat(name_width - name.chars().count() + 2)));
            // Bar, arrow, mark and its gap, name and its gap.
            let fixed = BAR.chars().count() + 2 + 2 + name_width + 2;
            let about: String = item
                .about
                .chars()
                .take(width.saturating_sub(fixed))
                .collect();
            let offset = name.chars().count() + 1;
            spans.extend(fuzzy::highlight(&about, &hits, offset, dim(), hit(dim())));
            panel_row(ACCENT, spans)
        })
        .collect()
}

/// A matched character: `style`, underlined and bold.
fn hit(style: Style) -> Style {
    style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}
