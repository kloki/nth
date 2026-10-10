//! The status bar under the input panel. Line 1 is general state: model,
//! effort, place (the working directory in magenta, then how many were
//! added with `/add-dir` as `(+N)`), how long the session has run, context
//! used as a percentage and what the session spent on the left, git branch and
//! status on the right, with the branch's pull request as a clickable
//! `#N`. Line 2 shows a hint about the last key or the queued
//! prompts on the left, and the running monitors and the language servers
//! that check a write on the right: a dot per server, coloured by its
//! state. The right side is cut first when a line is too narrow.

use std::{path::Path, time::SystemTime};

use nth_lsp::ServerState;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::UnicodeWidthStr;

use crate::{app::App, git};

/// Always this tall, whichever input panel is open.
pub const ROWS: u16 = 2;
/// In front of the share of input read from the prompt cache.
const CACHE: char = '↻';
/// In front of how full the context window is, a dot.
const CONTEXT: char = '◘';

/// Draws the two lines. Where the pull-request link ended up on line 1,
/// for a click to land on; `None` when none shows or it is cut off.
pub fn draw(frame: &mut Frame, area: Rect, app: &App) -> Option<Rect> {
    let [state, checks] = Layout::vertical([Constraint::Length(1); 2]).areas(area);

    let mut model = vec![app.model.clone()];
    model.extend(app.effort.wire().map(String::from));
    // The working directory, and how many were added with `/add-dir`,
    // say where the session works: magenta, against the bright white model
    // and effort. Only the count, since each path would crowd out the rest.
    let mut dirs = app.place.clone();
    if !app.extra_dirs.is_empty() {
        dirs.push_str(&format!(" (+{})", app.extra_dirs.len()));
    }
    let mut place = vec![
        Span::styled(model.join(" · "), bright_white()),
        Span::raw(" · "),
        Span::styled(dirs, magenta()),
        Span::raw(" "),
        Span::styled(
            session_time(SystemTime::now(), app.session_since),
            Style::new().fg(Color::Gray),
        ),
    ];
    // Unknown window, nothing; no reply yet, 0%.
    if let Some(window) = app.context_window().filter(|&w| w > 0) {
        let used = app.usage.map_or(0, |usage| usage.context());
        let (percent, colour) = context_usage(used, window);
        place.extend([
            Span::raw(" "),
            Span::styled(format!("{CONTEXT} {percent}%"), Style::new().fg(colour)),
        ]);
    }
    place.extend(spent(app));
    let mut summary = Vec::new();
    // The pull request on the branch, when the forge's tool found one:
    // its number in yellow, where it falls among the summary's spans.
    let mut link = None;
    if let Some(status) = &app.git {
        summary.push(Span::styled("git · ", bright_white()));
        if let Some(branch) = &status.branch {
            summary.push(Span::styled(branch.clone(), Style::new().fg(Color::Green)));
        }
        if let (Some(_), Some(pr)) = (&status.branch, &app.pr) {
            summary.push(Span::raw(" "));
            let text = format!("#{}", pr.number);
            let at = summary.iter().map(Span::width).sum();
            link = Some((at, text.width()));
            summary.push(Span::styled(text, Style::new().fg(Color::Yellow)));
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
    let width = summary.iter().map(Span::width).sum();
    let right = split_line(frame, state, place, summary);
    let link_area = link.and_then(|(at, wide)| link_rect(right, width, at, wide));
    let left = match &app.hint {
        Some(hint) => vec![Span::styled(hint.clone(), Style::new().fg(Color::Yellow))],
        None => queued(app),
    };
    split_line(frame, checks, left, servers(app));
    link_area
}

/// What the session spent, once it spent anything: its price at the
/// catalogue's rates when known, and how much of what it sent came from
/// the provider's prompt cache behind the cache icon, as ` $3.10 ↻ 82%`,
/// or `↻ ?` when the provider never said.
fn spent(app: &App) -> Vec<Span<'static>> {
    let total = app.spent.ledger.total();
    if total.steps == 0 {
        return Vec::new();
    }
    let cache = match total.tokens.cached_share() {
        Some(share) => format!("{CACHE} {:.0}%", share * 100.0),
        None => format!("{CACHE} ?"),
    };
    let text = match app.price() {
        Some(price) => format!(" {price} {cache}"),
        None => format!(" {cache}"),
    };
    vec![Span::styled(text, Style::new().fg(Color::Gray))]
}

/// Context used as a percentage of the window, coloured by how full it is:
/// white, yellow from half, magenta from four fifths.
fn context_usage(used: u64, window: u64) -> (u64, Color) {
    let percent = (used as f64 / window as f64 * 100.0).min(100.0).round() as u64;
    let colour = match percent {
        80..=100 => Color::Magenta,
        50..=79 => Color::Yellow,
        _ => Color::Gray,
    };
    (percent, colour)
}

/// How long the session has run: `24m`, `1h32m`, `2d3h`. A clock set back
/// before the session was created reads `0m` rather than failing.
fn session_time(now: SystemTime, since: SystemTime) -> String {
    let secs = now.duration_since(since).unwrap_or_default().as_secs();
    match secs {
        0..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h{}m", secs / 3_600, secs % 3_600 / 60),
        _ => format!("{}d{}h", secs / 86_400, secs % 86_400 / 3_600),
    }
}

/// Where a link drawn among the right side's spans ended up. They are
/// right-aligned, so it sits at its offset from where the side's content
/// starts: against its right edge, or at the area's left edge when the
/// side is cut, which takes the link with it when it reaches past the
/// area's right edge.
fn link_rect(right: Rect, width: usize, at: usize, link: usize) -> Option<Rect> {
    let edge = i32::from(right.right());
    let start = (edge - i32::try_from(width).unwrap_or(edge)).max(i32::from(right.x));
    let x = start + i32::try_from(at).unwrap_or(edge);
    let end = x + i32::try_from(link).unwrap_or(edge);
    (x >= i32::from(right.x) && end <= edge).then(|| {
        Rect::new(
            u16::try_from(x).unwrap_or(right.x),
            right.y,
            u16::try_from(link).unwrap_or(right.width),
            1,
        )
    })
}

/// Line 2, left: `⏵ 2 queued · <first line of the next prompt>`. Prompts
/// on their way into the running turn count too, and name the line: they
/// are the next thing the model hears.
fn queued(app: &App) -> Vec<Span<'static>> {
    let waiting = app.inbox.prompts_waiting();
    let count = waiting.as_ref().map_or(0, |(count, _)| *count) + app.queue.len();
    let first = match waiting {
        Some((_, first)) => first,
        None => match app.queue.front() {
            Some(next) => {
                let next = next.label();
                let first = next.lines().find(|line| !line.trim().is_empty());
                first.unwrap_or_default().to_string()
            }
            None => return Vec::new(),
        },
    };
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

/// Where the session works, as line 1 shows it.
fn magenta() -> Style {
    Style::new().fg(Color::Magenta)
}

/// Draws `left` against the left edge and `right` against the right edge
/// in what is left of the row, one column clear of it, and returns where
/// the right side was drawn.
pub fn split_line(
    frame: &mut Frame,
    area: Rect,
    left: Vec<Span<'static>>,
    right: Vec<Span<'static>>,
) -> Rect {
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
    right_area
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
    use std::time::Duration;

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

    #[test]
    fn session_time_reads_in_minutes_hours_and_days() {
        let now = SystemTime::now();
        let ago = |secs| session_time(now, now - Duration::from_secs(secs));

        assert_eq!(ago(0), "0m");
        assert_eq!(ago(59), "0m");
        assert_eq!(ago(60), "1m");
        assert_eq!(ago(24 * 60), "24m");
        assert_eq!(ago(3_600), "1h0m");
        assert_eq!(ago(3_600 + 32 * 60), "1h32m");
        assert_eq!(ago(86_400), "1d0h");
        assert_eq!(ago(2 * 86_400 + 3 * 3_600), "2d3h");
        // A clock before the session began reads zero, not a panic.
        assert_eq!(session_time(now - Duration::from_secs(5), now), "0m");
    }

    #[test]
    fn context_usage_warns_by_how_full_it_is() {
        let at = |used| context_usage(used, 1_000);
        let white = Color::Gray;

        assert_eq!(at(0), (0, white));
        assert_eq!(at(490), (49, white));
        assert_eq!(at(500), (50, Color::Yellow));
        assert_eq!(at(790), (79, Color::Yellow));
        assert_eq!(at(800), (80, Color::Magenta));
        assert_eq!(at(1_000), (100, Color::Magenta));
        // More than the window still caps at 100.
        assert_eq!(at(2_000), (100, Color::Magenta));
    }
}
