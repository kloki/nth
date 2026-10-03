//! What the agent is set up to do: its label and colour in the prompt.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Build,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Build => "build",
        }
    }

    /// Shared by the prompt's bar and label, and the spinner that replaces it.
    pub fn colour(self) -> Color {
        match self {
            Mode::Build => Color::Blue,
        }
    }
}
