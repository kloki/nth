//! Styling shared by every pane, so blocks and bars line up across them.

use ratatui::style::{Modifier, Style};

/// Marks a message block: chat entries and the prompt share it.
pub const BAR: &str = "▎ ";
pub const BAR_WIDTH: u16 = 2;
/// Compact lines are indented to sit under the text of a barred block.
pub const INDENT: &str = "  ";

pub fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}
