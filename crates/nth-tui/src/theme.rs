//! Styling shared by every pane, so blocks and bars line up across them.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Marks a message block: chat entries and the prompt share it.
pub const BAR: &str = "▎ ";
pub const BAR_WIDTH: u16 = 2;
/// Compact lines are indented to sit under the text of a barred block.
pub const INDENT: &str = "  ";

pub fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
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
