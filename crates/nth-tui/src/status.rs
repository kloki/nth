//! Two status rows: session and repository above, hint and widgets below.
//! The left side takes priority, with a gap before the right side. Directory
//! and branch copy targets and the PR and Curio links follow what is drawn.

use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

use nth_icons::icons;
use nth_lsp::ServerState;
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::{app::App, git};

/// Always this tall, whichever input panel is open.
pub const ROWS: u16 = 2;

/// Draws the two lines. Where the working directory, branch,
/// pull-request link and Curio ended up, for the pointer to land on;
/// `None` when one does not show or is cut off.
#[derive(Default)]
pub struct HitAreas {
    pub place: Option<Rect>,
    pub branch: Option<Rect>,
    pub pr: Option<Rect>,
    pub curio: Option<Rect>,
}

pub fn draw(frame: &mut Frame, area: Rect, app: &App) -> HitAreas {
    let [state, checks] = Layout::vertical([Constraint::Length(1); 2]).areas(area);

    let dirs_width = app.place.width();
    let mut place = vec![
        Span::styled(app.place.clone(), magenta()),
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
            Span::styled(
                format!("{} {percent}%", icons().context),
                Style::new().fg(colour),
            ),
        ]);
    }
    place.extend(spent(app));
    let mut summary = Vec::new();
    // The pull request on the branch, when the forge's tool found one:
    // its number in yellow, where it falls among the summary's spans.
    let mut link = None;
    let mut branch_at = None;
    if let Some(status) = &app.git {
        summary.push(Span::styled("git · ", bright_white()));
        if let Some(branch) = &status.branch {
            let at = summary.iter().map(Span::width).sum();
            branch_at = Some((at, branch.width()));
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
        Some(hint) => hint.spans(),
        None => queued(app),
    };
    let left_width = left
        .iter()
        .map(Span::width)
        .sum::<usize>()
        .min(checks.width as usize);
    let available =
        (checks.width as usize).saturating_sub(left_width + usize::from(left_width > 0));
    let (widgets, curio) = widgets(app, available);
    let widget_width = widgets.iter().map(Span::width).sum();
    let widget_area = split_line(frame, checks, left, widgets);
    // The left side starts at the line's edge, cut at its end.
    let place_width = u16::try_from(dirs_width)
        .unwrap_or(u16::MAX)
        .min(state.width);
    HitAreas {
        place: (place_width > 0).then_some(Rect {
            width: place_width,
            ..state
        }),
        branch: branch_at.and_then(|(at, wide)| link_rect(right, width, at, wide)),
        pr: link_area,
        curio: curio.and_then(|(at, wide)| link_rect(widget_area, widget_width, at, wide)),
    }
}

/// What the session spent at the catalogue's rates, once it spent
/// anything and the price is known, as ` $3.10`.
fn spent(app: &App) -> Vec<Span<'static>> {
    if app.spent.ledger.total().steps == 0 {
        return Vec::new();
    }
    match app.price() {
        Some(price) => vec![Span::styled(
            format!(" {price}"),
            Style::new().fg(Color::Yellow),
        )],
        None => Vec::new(),
    }
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
    (right.height > 0 && link > 0 && x >= i32::from(right.x) && end <= edge).then(|| {
        Rect::new(
            u16::try_from(x).unwrap_or(right.x),
            right.y,
            u16::try_from(link).unwrap_or(right.width),
            1,
        )
    })
}

/// Line 2, left: `> 2 queued · <first line of the next prompt>`. Prompts
/// on their way into the running turn count too, and name the line: they
/// are the next thing the model hears. So does a turn waiting for a save.
fn queued(app: &App) -> Vec<Span<'static>> {
    let waiting = app.inbox.prompts_waiting();
    // A wake for notices is not something you sent.
    let held = app.turn_after_save.as_ref().filter(|held| !held.is_wake());
    let count = waiting.as_ref().map_or(0, |(count, _)| *count)
        + usize::from(held.is_some())
        + app.queue.len();
    let first = match waiting {
        Some((_, first)) => first,
        None => match held.or(app.queue.front()) {
            Some(next) => {
                let next = next.label();
                let first = next.lines().find(|line| !line.trim().is_empty());
                first.unwrap_or_default().to_string()
            }
            None => return Vec::new(),
        },
    };
    let line = format!("{} {count} queued · {first}", icons().queued);
    vec![Span::styled(line, Style::new().fg(Color::Gray))]
}

/// The lower-right widgets reserve space for LSP and Curio before directories.
fn widgets(app: &App, available: usize) -> (Vec<Span<'static>>, Option<(usize, usize)>) {
    let mut tail = Vec::new();
    if let Some(state) = app.servers.iter().map(|s| &s.state).max_by_key(|s| rank(s)) {
        tail.push(Span::styled(
            format!("{} ", icons().dot),
            Style::new().fg(state_colour(state)),
        ));
        tail.push(Span::styled("lsp", Style::new().fg(Color::Gray)));
    }
    let tail_width: usize = tail.iter().map(Span::width).sum();
    let reserved = tail_width
        + usize::from(app.has_curios())
        + usize::from(tail_width > 0 && app.has_curios());
    let budget = available.saturating_sub(reserved + usize::from(reserved > 0));
    let mut spans = directories(
        &app.extra_dirs,
        if reserved == 0 { available } else { budget },
    );
    if !spans.is_empty() && !tail.is_empty() {
        spans.push(Span::raw(" "));
    }
    spans.extend(tail);
    let curio = if app.has_curios() {
        let at = spans.iter().map(Span::width).sum::<usize>();
        let wide = if spans.is_empty() {
            1
        } else {
            spans.push(Span::raw(" "));
            2
        };
        spans.push(Span::styled(icons().curio, Style::new().fg(Color::Cyan)));
        Some((at, wide))
    } else {
        None
    };
    (spans, curio)
}

/// Shortest suffixes that distinguish directory names from one another.
fn directory_names(paths: &[PathBuf]) -> Vec<String> {
    let parts: Vec<Vec<String>> = paths
        .iter()
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        })
        .collect();
    parts
        .iter()
        .enumerate()
        .map(|(i, path)| {
            for n in 1..=path.len() {
                let suffix = &path[path.len() - n..];
                if parts
                    .iter()
                    .enumerate()
                    .all(|(j, other)| i == j || !other.ends_with(suffix))
                {
                    return suffix.join("/");
                }
            }
            paths[i].display().to_string()
        })
        .collect()
}

fn directories(paths: &[PathBuf], budget: usize) -> Vec<Span<'static>> {
    let names = directory_names(paths);
    if names.is_empty() {
        return Vec::new();
    }
    let mut shown = Vec::new();
    let mut used = 2; // "+ "
    for (i, name) in names.iter().enumerate() {
        let separator = if i == 0 { 0 } else { 2 };
        let remaining = names.len() - i - 1;
        let overflow = if remaining > 0 {
            2 + format!("+{remaining}").width()
        } else {
            0
        };
        if used + separator + name.width() + overflow <= budget {
            used += separator + name.width();
            shown.push(name.clone());
        } else {
            // Only shorten the first name; later names remain whole.
            if i == 0 {
                let room = budget.saturating_sub(used + overflow);
                if room >= 2 {
                    let mut short = String::new();
                    for ch in name.chars() {
                        if short.width() + ch.width().unwrap_or(0) + 1 > room {
                            break;
                        }
                        short.push(ch);
                    }
                    short.push('…');
                    shown.push(short);
                }
            }
            break;
        }
    }
    let omitted = names.len() - shown.len();
    if shown.is_empty() {
        let text = format!("+{omitted}");
        return if text.width() <= budget {
            vec![Span::raw(text)]
        } else {
            Vec::new()
        };
    }
    let mut spans = vec![Span::raw("+ ")];
    for (i, name) in shown.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(", "));
        }
        spans.push(Span::styled(name, magenta()));
    }
    if omitted > 0 {
        spans.push(Span::raw(format!(", +{omitted}")));
    }
    spans
}

/// Which of a server's clients speaks for it: a broken one over a
/// starting one over a connected one.
fn rank(state: &ServerState) -> u8 {
    match state {
        ServerState::Connected => 0,
        ServerState::Starting => 1,
        ServerState::Broken(_) => 2,
    }
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
    fn directory_widget_disambiguates_and_fits_unicode() {
        let paths: Vec<PathBuf> = ["/one/docs", "/two/docs", "/src/界面"]
            .into_iter()
            .map(PathBuf::from)
            .collect();
        assert_eq!(directory_names(&paths), ["one/docs", "two/docs", "界面"]);
        let text = |width| Line::from(directories(&paths, width)).to_string();
        assert_eq!(text(40), "+ one/docs, two/docs, 界面");
        assert_eq!(text(14), "+ one/docs, +2");
        assert_eq!(text(9), "+ on…, +2");
        assert_eq!(text(2), "+3");
        for width in 0..40 {
            assert!(text(width).width() <= width);
        }
    }

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
