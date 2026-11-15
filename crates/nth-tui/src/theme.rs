//! Styling shared by the three bands and the content panel's tabs, so
//! blocks and bars line up across them.

use nth_protocol::Mode;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use crate::app::TabState;

/// Marks a message block: chat entries and the prompt share it.
pub const BAR: &str = "▎ ";
pub const BAR_WIDTH: u16 = 2;
/// Compact lines are indented to sit under the text of a barred block.
pub const INDENT: &str = "  ";

pub fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// A mode's colour, shared by the prompt's bar and label and the spinner
/// that replaces the label: plan is blue, act magenta.
pub fn mode_colour(mode: Mode) -> Color {
    match mode {
        Mode::Plan => Color::Blue,
        Mode::Act => Color::Magenta,
    }
}

/// The prompt's colour while it holds a command to run.
pub const SHELL_COLOUR: Color = Color::Yellow;
/// The prompt's colour while a primary agent runs in place of plan or
/// act, and the agent picker's.
pub const AGENT_COLOUR: Color = Color::Green;
/// The prompt's colour while it talks to a subagent: a model talking to
/// you, as the question panel is.
pub const SUBAGENT_COLOUR: Color = Color::Cyan;
/// The provider prefix of a model: bright white, against the model's
/// blue.
pub const PROVIDER_COLOUR: Color = Color::White;

/// A model as the prompt, the work tab and a subagent's header name it:
/// its `provider/` prefix in bright white, the model id in blue, and the
/// effort after a ` · ` when it runs at one. The effort is its name, since
/// a work item's file keeps only that.
pub fn model_spans(model: &str, effort: Option<&str>) -> Vec<Span<'static>> {
    let blue = Style::new().fg(Color::Blue);
    let mut spans = match model
        .split_once('/')
        .filter(|(provider, _)| !provider.is_empty())
    {
        Some((provider, id)) => vec![
            Span::styled(format!("{provider}/"), Style::new().fg(PROVIDER_COLOUR)),
            Span::styled(id.to_string(), blue),
        ],
        None => vec![Span::styled(model.to_string(), blue)],
    };
    if let Some(effort) = effort {
        spans.push(Span::styled(
            format!(" · {effort}"),
            Style::new().fg(Color::Gray),
        ));
    }
    spans
}

/// A tab's colour in the header; `None` leaves the terminal's own.
pub fn tab_colour(state: TabState) -> Option<Color> {
    match state {
        TabState::Idle => None,
        TabState::Working => Some(Color::Blue),
        TabState::Done => Some(Color::Green),
        TabState::Failed => Some(Color::Red),
        TabState::NeedsYou => Some(Color::Magenta),
    }
}

/// A header tab or a chat link under the pointer: the colour of what is
/// picked, as in a list.
pub const HOVER: Color = Color::Magenta;
/// The status bar's links under the pointer: bright white, against their
/// own yellow and cyan.
pub const HOVER_STATUS: Color = Color::White;
/// Behind the chat entry under the pointer: bright black, visible on any
/// theme, with dim text still readable on it.
pub const HOVER_ENTRY: Color = Color::DarkGray;

/// The highlighted item in any list: popup, picker.
pub fn pick() -> Style {
    Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD)
}

// Every input panel (the prompt, the model picker, and whatever swaps in
// for them next) has one shape, so they read as the same kind of thing:
// - the default background and no border;
// - one accent colour, for the bar down every row and the title on top,
//   which is plain rather than bold so the content stays the loudest;
// - its content on the rows under the title, after the bar.

/// The top row of an input panel: the bar and the title in its accent.
pub fn panel_title(title: &str, accent: Color) -> Line<'_> {
    let style = Style::new().fg(accent);
    Line::from(vec![Span::styled(BAR, style), Span::styled(title, style)])
}

/// Any other row of an input panel: the accent bar, then `content`.
pub fn panel_row<'a>(accent: Color, content: impl IntoIterator<Item = Span<'a>>) -> Line<'a> {
    let mut spans = vec![Span::styled(BAR, Style::new().fg(accent))];
    spans.extend(content);
    Line::from(spans)
}

/// The bar down a section of the diagnostics, usage and context tabs: grey,
/// so the sections read as the chat's blocks do without competing with them.
pub const SECTION_COLOUR: Color = Color::DarkGray;

/// The diagnostics and usage tabs' sections as barred blocks: the section
/// bar on every row of each, blank ones too, joined as `join` does.
pub fn sections(blocks: Vec<Vec<Line<'static>>>) -> Vec<Line<'static>> {
    let bar = Style::new().fg(SECTION_COLOUR);
    join(
        blocks
            .into_iter()
            .map(|block| {
                block
                    .into_iter()
                    .map(|mut line| {
                        line.spans.insert(0, Span::styled(BAR, bar));
                        line
                    })
                    .collect()
            })
            .collect(),
    )
}

/// Barred blocks with one bare blank line between them, as between chat
/// entries.
pub fn join(blocks: Vec<Vec<Line<'static>>>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for block in blocks {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(block);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(spans: &[Span<'_>]) -> String {
        spans.iter().map(|span| span.content.as_ref()).collect()
    }

    #[test]
    fn the_model_follows_the_provider_and_the_effort_the_model() {
        let spans = model_spans("opencode/glm-5.3", Some("high"));

        assert_eq!(text(&spans), "opencode/glm-5.3 · high");
        assert_eq!(spans[0].content, "opencode/");
        assert_eq!(spans[0].style.fg, Some(PROVIDER_COLOUR));
        assert_eq!(spans[1].style.fg, Some(Color::Blue));
        assert_eq!(spans[2].style.fg, Some(Color::Gray));
    }

    #[test]
    fn a_model_without_a_provider_shows_whole() {
        let spans = model_spans("glm", None);

        assert_eq!(text(&spans), "glm");
        assert_eq!(spans[0].style.fg, Some(Color::Blue));
    }

    #[test]
    fn only_the_first_slash_splits_the_provider() {
        let spans = model_spans("openrouter/z-ai/glm-5.2", None);

        assert_eq!(spans[0].content, "openrouter/");
        assert_eq!(spans[1].content, "z-ai/glm-5.2");
        assert_eq!(text(&spans), "openrouter/z-ai/glm-5.2");
    }
}
