//! Draws the picker in the prompt's place: a header with its keys, then the
//! sessions, scrolled to the highlighted one.

use std::time::SystemTime;

use nth_session::Summary;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{SessionPicker, State, age};
use crate::{
    status,
    theme::{BAR, dim, panel_row, panel_title, pick},
};

const TITLE: &str = "resume session";
const KEYS: &str = "↑↓ session · enter · esc";

/// The picker's bar and title; see `theme::panel_title`.
const ACCENT: Color = Color::Magenta;
/// Room for the widest age, `999d`.
const AGE_WIDTH: usize = 4;

pub fn draw(frame: &mut Frame, area: Rect, picker: &SessionPicker) {
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
        State::Loading => vec![note(Span::styled("loading sessions…", dim()))],
        State::Failed(error) => vec![note(Span::styled(
            format!("✗ {error}"),
            Style::new().fg(Color::Red),
        ))],
        State::Ready { sessions, selected } => rows(
            picker,
            sessions,
            *selected,
            usize::from(list.width),
            usize::from(list.height),
        ),
    };
    frame.render_widget(Paragraph::new(lines), list);
}

fn note(text: Span<'_>) -> Line<'_> {
    panel_row(ACCENT, [text])
}

/// One row per session: how long ago it was used, a ✓ on the one in use,
/// its title, and the directory it runs in.
fn rows<'a>(
    picker: &SessionPicker,
    sessions: &'a [Summary],
    selected: usize,
    width: usize,
    height: usize,
) -> Vec<Line<'a>> {
    let now = SystemTime::now();
    let home = std::env::var("HOME").ok();
    let first = selected.saturating_sub(height.saturating_sub(1));
    let pick = pick();

    sessions
        .iter()
        .enumerate()
        .skip(first)
        .take(height)
        .map(|(i, session)| {
            let (arrow, title) = if i == selected {
                ("→ ", pick)
            } else {
                ("  ", Style::new().fg(Color::Blue))
            };
            let active = if session.id == picker.current {
                "✓"
            } else {
                " "
            };
            let place = status::place(&session.cwd, home.as_deref());
            // Bar, arrow, age and its gap, mark and its gap, then two
            // spaces before the place.
            let fixed = BAR.chars().count() + 2 + AGE_WIDTH + 1 + 2 + 2;
            let room = width.saturating_sub(fixed + place.chars().count());
            let text: String = session.title.chars().take(room).collect();
            panel_row(
                ACCENT,
                [
                    Span::styled(arrow, pick),
                    Span::styled(
                        format!("{:>AGE_WIDTH$} ", age(now, session.updated_at)),
                        dim(),
                    ),
                    Span::styled(active, Style::new().fg(Color::Green)),
                    Span::raw(" "),
                    Span::styled(format!("{text:<room$}  "), title),
                    Span::styled(place, dim()),
                ],
            )
        })
        .collect()
}
