//! The status bar under the input panel. General state is right-aligned:
//! model, place and branch on line 1, git status on line 2. The left side
//! shows what the running turn is doing and the one hint that matters now,
//! and is cut first when a line is too narrow.

use std::path::Path;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{app::App, chat::Activity, git, theme::dim};

/// Always this tall, whichever input panel is open.
pub const ROWS: u16 = 2;

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let [now, hints] = Layout::vertical([Constraint::Length(1); 2]).areas(area);
    let busy = app.busy_since.is_some();

    let model = match app.effort.wire() {
        Some(effort) => format!("{} · {effort}", app.model),
        None => app.model.clone(),
    };
    let mut place = vec![
        Span::styled(model, Style::new().fg(Color::Blue)),
        Span::raw(" "),
        Span::styled(app.place.clone(), Style::new().fg(Color::Red)),
    ];
    if let Some(branch) = app.git.as_ref().and_then(|git| git.branch.clone()) {
        place.extend([
            Span::raw(" "),
            Span::styled(branch, Style::new().fg(Color::Green)),
        ]);
    }
    let mut activity = Vec::new();
    if busy {
        match app.chat.transcript.activity() {
            Activity::Thinking => activity.push(Span::styled("thinking", dim())),
            Activity::Writing => activity.push(Span::styled("writing", dim())),
            Activity::Tool(call) => activity.extend([
                Span::styled(call.name.clone(), Style::new().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(call.summary(&app.cwd), dim()),
            ]),
        }
    }
    split_line(frame, now, activity, place);

    let below = app.chat.lines_below();
    let hint = if below > 0 {
        vec![Span::styled(
            format!("↓ {below} more · ctrl+End"),
            Style::new().fg(Color::Yellow),
        )]
    } else if busy {
        vec![Span::styled("esc to interrupt", dim())]
    } else {
        Vec::new()
    };
    let summary = app.git.as_ref().map(git::summary).unwrap_or_default();
    split_line(frame, hints, hint, summary);
}

/// Draws `right` against the right edge and `left` in what is left of the
/// row, one column clear of it.
fn split_line(frame: &mut Frame, area: Rect, left: Vec<Span<'static>>, right: Vec<Span<'static>>) {
    let right = Line::from(right);
    let width = u16::try_from(right.width())
        .unwrap_or(u16::MAX)
        .min(area.width);
    let [left_area, _, right_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(u16::from(width > 0)),
        Constraint::Length(width),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(right), right_area);
    frame.render_widget(Paragraph::new(Line::from(left)), left_area);
}

/// `cwd` as shown in the status bar, with `home` written as `~`.
pub fn place(cwd: &Path, home: Option<&str>) -> String {
    match home {
        Some(home) if !home.is_empty() => match cwd.strip_prefix(home) {
            Ok(rest) => format!("~/{}", rest.display())
                .trim_end_matches('/')
                .to_string(),
            Err(_) => cwd.display().to_string(),
        },
        _ => cwd.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_abbreviates_home_by_path_components() {
        let place = |cwd: &str, home| place(cwd.as_ref(), home);

        assert_eq!(place("/home/k", Some("/home/k")), "~");
        assert_eq!(place("/home/k/repo", Some("/home/k")), "~/repo");
        assert_eq!(place("/home/kate", Some("/home/k")), "/home/kate");
        assert_eq!(place("/repo", Some("")), "/repo");
        assert_eq!(place("/repo", None), "/repo");
    }
}
