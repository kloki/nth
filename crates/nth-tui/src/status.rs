//! The status row between chat and prompt: what the running turn is doing
//! on the left; a scroll hint, the interrupt hint, or model and place on
//! the right.

use std::path::Path;

use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

use crate::{app::App, chat::Activity, theme::dim};

pub fn draw(frame: &mut Frame, area: Rect, app: &App) {
    let busy = app.busy_since.is_some();
    if busy {
        let mut spans = Vec::new();
        match app.chat.transcript.activity() {
            Activity::Thinking => spans.push(Span::styled("thinking", dim())),
            Activity::Writing => spans.push(Span::styled("writing", dim())),
            Activity::Tool(call) => spans.extend([
                Span::styled(call.name.clone(), Style::new().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(call.summary(&app.cwd), dim()),
            ]),
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    let below = app.chat.lines_below();
    let right = if below > 0 {
        Span::styled(
            format!("↓ {below} more · ctrl+End"),
            Style::new().fg(Color::Yellow),
        )
    } else if busy {
        Span::styled("esc to interrupt", dim())
    } else {
        let model = match app.effort.wire() {
            Some(effort) => format!("{} · {effort}", app.model),
            None => app.model.clone(),
        };
        Span::styled(format!("{model} · {}", app.place), dim())
    };
    frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
}

/// `cwd` as shown in the status row, with `home` written as `~`.
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
