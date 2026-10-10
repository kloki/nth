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
/// The prompt's colour while it talks to a subagent: a model talking to
/// you, as the question panel is.
pub const SUBAGENT_COLOUR: Color = Color::Cyan;
/// The provider prefix in the prompt's label: bright white, against the
/// model's blue.
pub const PROVIDER_COLOUR: Color = Color::White;

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
