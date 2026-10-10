//! Draws the picker in the prompt's place: a header with its keys, the
//! query, then the sessions matching it, newest first while it is empty,
//! scrolled to the highlighted one.

use std::time::SystemTime;

use nth_icons::icons;
use nth_session::Summary;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{SessionPicker, State, age, haystack};
use crate::{
    fuzzy::{self, Filter, query_row},
    status,
    theme::{BAR, dim, panel_row, panel_title, pick},
};

const TITLE: &str = "resume session";
const KEYS: &str = "type to filter · ↑↓ session · enter · esc";

/// The picker's bar and title; see `theme::panel_title`.
const ACCENT: Color = Color::Magenta;
/// Room for the widest age, `999d`.
const AGE_WIDTH: usize = 4;

pub fn draw(frame: &mut Frame, area: Rect, picker: &SessionPicker) {
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
        State::Loading => (vec![note(Span::styled("loading sessions…", dim()))], body),
        State::Failed(error) => (
            vec![note(Span::styled(
                format!("{} {error}", icons().fail),
                Style::new().fg(Color::Red),
            ))],
            body,
        ),
        State::Ready {
            sessions,
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
                sessions.len(),
            );
            let lines = rows(
                picker,
                Listed {
                    sessions,
                    filter,
                    shown,
                },
                *selected,
                usize::from(list.width),
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
    sessions: &'a [Summary],
    filter: &'a Filter,
    shown: &'a [usize],
}

/// One row per session matching the query: how long ago it was used, a ✓
/// on the one in use, its title, and the directory it runs in, with the
/// characters the query matched marked.
fn rows<'a>(
    picker: &SessionPicker,
    listed: Listed<'_>,
    selected: usize,
    width: usize,
    height: usize,
) -> Vec<Line<'a>> {
    let Listed {
        sessions,
        filter,
        shown,
    } = listed;
    if shown.is_empty() {
        return vec![note(Span::styled("  no matches", dim()))];
    }
    let now = SystemTime::now();
    let home = std::env::var("HOME").ok();
    let first = selected.saturating_sub(height.saturating_sub(1));
    let pick = pick();

    shown
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(row, &i)| {
            let session = &sessions[i];
            let (arrow, title) = if row == selected {
                (format!("{} ", icons().pick), pick)
            } else {
                ("  ".to_string(), Style::new().fg(Color::Blue))
            };
            let active = if session.id == picker.current {
                icons().ok
            } else {
                " "
            };
            let place = status::place(&session.cwd, home.as_deref());
            // Bar, arrow, age and its gap, mark and its gap, then two
            // spaces before the place.
            let fixed = BAR.chars().count() + 2 + AGE_WIDTH + 1 + 2 + 2;
            let room = width.saturating_sub(fixed + place.chars().count());
            let text: String = session.title.chars().take(room).collect();
            let shown = text.chars().count();
            let hits = filter.indices(&haystack(session, home.as_deref()));
            let mut spans = vec![
                Span::styled(arrow, pick),
                Span::styled(
                    format!("{:>AGE_WIDTH$} ", age(now, session.updated_at)),
                    dim(),
                ),
                Span::styled(active, Style::new().fg(Color::Green)),
                Span::raw(" "),
            ];
            spans.extend(fuzzy::highlight(&text, &hits, 0, title, hit(title)));
            spans.push(Span::raw(" ".repeat(room - shown + 2)));
            let offset = session.title.chars().count() + 1;
            spans.extend(fuzzy::highlight(&place, &hits, offset, dim(), hit(dim())));
            panel_row(ACCENT, spans)
        })
        .collect()
}

/// A matched character: `style`, underlined and bold.
fn hit(style: Style) -> Style {
    style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
}
