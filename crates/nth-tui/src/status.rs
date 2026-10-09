//! The status bar under the input panel. Line 1 is general state: model,
//! place and context used on the left, git branch and status on the right.
//! Line 2 shows a hint about the last key or the queued prompts on the
//! left, and the running monitors and the language servers that check a
//! write on the right: a dot per server, coloured by its state. The right
//! side is cut first when a line is too narrow.

use std::path::Path;

use braille_bar::BrailleBar;
use nth_lsp::ServerState;
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
/// Characters in the context bar.
const BAR_WIDTH: usize = 13;

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let [state, checks] = Layout::vertical([Constraint::Length(1); 2]).areas(area);

    let mut place = vec![app.model.clone()];
    place.extend(app.effort.wire().map(String::from));
    place.push(app.place.clone());
    let mut place = vec![Span::styled(place.join(" · "), bright_white())];
    // Unknown window, no bar; no reply yet, an empty one.
    if let Some(window) = app.context_window().filter(|&w| w > 0) {
        let used = app.usage.map_or(0, |usage| usage.context());
        let percent = (used as f64 / window as f64 * 100.0).min(100.0);
        place.extend([
            Span::raw(" "),
            Span::styled(
                BrailleBar::new(BAR_WIDTH).render(percent),
                Style::new().fg(Color::Gray),
            ),
        ]);
    }
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
    let left = match &app.hint {
        Some(hint) => vec![Span::styled(hint.clone(), Style::new().fg(Color::Yellow))],
        None => queued(app),
    };
    split_line(frame, checks, left, servers(app));
}

/// Line 2, left: `⏵ 2 queued · <first line of the next prompt>`. Prompts
/// on their way into the running turn count too, and name the line: they
/// are the next thing the model hears.
fn queued(app: &App) -> Vec<Span<'static>> {
    let mut prompts = app.inbox.pending_prompts().into_iter();
    let count = prompts.len() + app.queue.len();
    let next = prompts
        .next()
        .or_else(|| app.queue.front().map(|next| next.label()));
    let Some(next) = next else {
        return Vec::new();
    };
    let first = next
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let line = format!("⏵ {count} queued · {first}");
    vec![Span::styled(line, Style::new().fg(Color::Gray))]
}

/// Line 2, right: `● rust  ● typescript`.
fn servers(app: &App) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for server in &app.servers {
        let colour = state_colour(&server.state);
        if !spans.is_empty() {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled("● ", Style::new().fg(colour)));
        spans.push(Span::styled(
            server.id.clone(),
            Style::new().fg(Color::Gray),
        ));
    }
    spans
}

/// A server's dot: green when connected, yellow while starting, red when
/// broken.
pub fn state_colour(state: &ServerState) -> Color {
    match state {
        ServerState::Connected => Color::Green,
        ServerState::Starting => Color::Yellow,
        ServerState::Broken(_) => Color::Red,
    }
}

/// `Color::White` is the terminal's bright white; plain white is `Gray`.
fn bright_white() -> Style {
    Style::new().fg(Color::White)
}

/// Draws `left` against the left edge and `right` against the right edge
/// in what is left of the row, one column clear of it.
pub fn split_line(
    frame: &mut Frame,
    area: Rect,
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
) {
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
