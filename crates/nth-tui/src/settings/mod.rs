//! How the chats show what the model did, and the panel that changes it.
//! ChatSettings last for this run of nth only: they are never written to the
//! config or the saved session.

mod view;

pub use view::draw;

/// What the chats show, for this run only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChatSettings {
    /// The model's reasoning under its `∴` line.
    pub thinking: bool,
    /// What each tool call returned, under its row.
    pub tool_output: bool,
}

impl Default for ChatSettings {
    fn default() -> Self {
        Self {
            thinking: true,
            tool_output: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setting {
    Thinking,
    ToolOutput,
}

impl Setting {
    pub const ALL: [Setting; 2] = [Setting::Thinking, Setting::ToolOutput];

    pub fn name(self) -> &'static str {
        match self {
            Setting::Thinking => "thinking",
            Setting::ToolOutput => "tool output",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Setting::Thinking => "the model's reasoning under ∴",
            Setting::ToolOutput => "what each tool call returned",
        }
    }

    pub fn get(self, settings: ChatSettings) -> bool {
        match self {
            Setting::Thinking => settings.thinking,
            Setting::ToolOutput => settings.tool_output,
        }
    }

    pub fn flip(self, settings: &mut ChatSettings) {
        match self {
            Setting::Thinking => settings.thinking ^= true,
            Setting::ToolOutput => settings.tool_output ^= true,
        }
    }
}

/// The panel's title and a row per setting.
pub const ROWS: u16 = 1 + Setting::ALL.len() as u16;

/// Which setting is highlighted; the values live on the app, so a change
/// shows in the chat behind the panel at once.
#[derive(Debug, Default)]
pub struct SettingsPanel {
    selected: usize,
}

impl SettingsPanel {
    pub fn next(&mut self) {
        self.selected = (self.selected + 1).min(Setting::ALL.len() - 1);
    }

    pub fn prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn selected(&self) -> Setting {
        Setting::ALL[self.selected]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_within_the_settings() {
        let mut panel = SettingsPanel::default();
        panel.prev();
        assert_eq!(panel.selected(), Setting::Thinking);
        panel.next();
        panel.next();
        assert_eq!(panel.selected(), Setting::ToolOutput);
    }

    #[test]
    fn flips_one_setting() {
        let mut settings = ChatSettings::default();
        Setting::ToolOutput.flip(&mut settings);
        assert!(settings.thinking);
        assert!(!Setting::ToolOutput.get(settings));
    }
}
