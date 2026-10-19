//! The settings panel: how the chats show what the model did, flipped for
//! this run only.

use super::{App, input::Input, keys::Action};
use crate::settings::SettingsPanel;

impl App {
    /// `/settings`: the panel takes the prompt's place.
    pub(super) fn open_settings(&mut self) {
        self.completion = None;
        self.input = Input::Settings(SettingsPanel::default());
    }

    /// A key while the settings panel is open.
    pub(super) fn key_settings(&mut self, action: Action) {
        let Input::Settings(panel) = &mut self.input else {
            return;
        };
        match action {
            Action::SelectNext => panel.next(),
            Action::SelectPrev => panel.prev(),
            Action::Submit | Action::Insert(' ') => panel.selected().flip(&mut self.settings),
            Action::Interrupt | Action::ClearOrQuit | Action::LlmPicker => {
                self.input = Input::Prompt
            }
            _ => {}
        }
    }
}
