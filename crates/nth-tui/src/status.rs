//! The status bar under the input panel. Line 1 is general state: model
//! and place on the left, git branch and status on the right. Line 2 holds the
//! one hint that matters now on the right. The right side is cut first when
//! a line is too narrow.

use std::path::Path;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{app::App, git};

/// Always this tall, whichever input panel is open.
pub const ROWS: u16 = 2;

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let [state, now] = Layout::vertical([Constraint::Length(1); 2]).areas(area);

    let mut place = vec![app.model.clone()];
    place.extend(app.effort.wire().map(String::from));
    place.push(app.place.clone());
    let place = vec![Span::styled(place.join(" · "), bright_white())];
    let mut summary = Vec::new();
    if let Some(status) = &app.git {
        summary.push(Span::styled("git · ", bright_white()));
        if let Some(branch) = &status.branch {
            summary.push(Span::styled(branch.clone(), Style::new().fg(Color::Green)));
        }
        let counts = git::summary(status);
        if status.branch.is_some() && !counts.is_empty() {
            summary.push(Span::raw(" "));
        }
        summary.extend(counts);
        if summary.len() == 1 {
            summary.clear();
        }
    }
    split_line(frame, state, place, summary);

    let below = app.chat.lines_below();
    let hint = if below > 0 {
        vec![Span::styled(
            format!("↓ {below} more · ctrl+End"),
            Style::new().fg(Color::Yellow),
        )]
    } else {
        Vec::new()
    };
    split_line(frame, now, Vec::new(), hint);
}

/// `Color::White` is the terminal's bright white; plain white is `Gray`.
fn bright_white() -> Style {
    Style::new().fg(Color::White)
}

/// Draws `left` against the left edge and `right` against the right edge
/// in what is left of the row, one column clear of it.
fn split_line(frame: &mut Frame, area: Rect, left: Vec<Span<'static>>, right: Vec<Span<'static>>) {
    let left = Line::from(left);
    let width = u16::try_from(left.width())
        .unwrap_or(u16::MAX)
        .min(area.width);
    let [left_area, _, right_area] = Layout::horizontal([
        Constraint::Length(width),
        Constraint::Length(u16::from(width > 0)),
        Constraint::Min(0),
    ])
    .areas(area);
    frame.render_widget(Paragraph::new(left), left_area);
    frame.render_widget(
        Paragraph::new(Line::from(right).right_aligned()),
        right_area,
    );
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
