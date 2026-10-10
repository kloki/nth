//! The git summary on the status bar's second line, after the starship
//! config in docs/ui.md: each part only when it is non-zero.

use nth_icons::icons;
use ratatui::{
    style::{Color, Style},
    text::Span,
};

use super::GitStatus;

/// Empty for a clean tree that is level with its upstream.
pub fn summary(status: &GitStatus) -> Vec<Span<'static>> {
    let icons = &icons().git;
    let white = Style::new().fg(Color::Gray);
    let yellow = Style::new().fg(Color::Yellow);
    let mut parts: Vec<(String, Style)> = Vec::new();
    if status.conflicted > 0 {
        parts.push((icons.conflicted.into(), Style::new().fg(Color::Red)));
    }
    match (status.ahead, status.behind) {
        (0, 0) => {}
        (ahead, 0) => parts.push((format!("+{ahead}"), yellow)),
        (0, behind) => parts.push((format!("-{behind}"), yellow)),
        _ => parts.push((icons.diverged.into(), white)),
    }
    let counted = [
        (status.modified, "*", Color::Magenta),
        (status.renamed, icons.renamed, Color::Yellow),
        (status.deleted, icons.deleted, Color::Red),
        (status.staged, icons.staged, Color::Blue),
        (status.untracked, icons.untracked, Color::Gray),
    ];
    for (count, icon, colour) in counted {
        if count > 0 {
            // `*` hugs its count; the icons get a space, as in starship.
            let gap = if icon == "*" { "" } else { " " };
            parts.push((format!("{icon}{gap}{count}"), Style::new().fg(colour)));
        }
    }
    if status.stashed {
        parts.push((icons.stashed.into(), white));
    }

    let mut spans = Vec::new();
    for (i, (text, style)) in parts.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(text, style));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(status: &GitStatus) -> String {
        summary(status).iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn shows_only_what_is_non_zero_in_starship_order() {
        assert_eq!(text(&GitStatus::default()), "");
        let status = GitStatus {
            ahead: 3,
            modified: 4,
            staged: 2,
            untracked: 1,
            ..GitStatus::default()
        };
        assert_eq!(text(&status), "+3 *4 ✚ 2 ? 1");
    }

    #[test]
    fn diverged_replaces_ahead_and_behind() {
        let status = GitStatus {
            ahead: 2,
            behind: 1,
            conflicted: 1,
            stashed: true,
            ..GitStatus::default()
        };
        assert_eq!(text(&status), "= ⇕ $");
    }
}
