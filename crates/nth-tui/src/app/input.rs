//! The input panel between the content panel and the status bar: the
//! prompt, or a widget that swaps in for it and hands back to the prompt
//! when done.

use ratatui::layout::Rect;

use crate::{
    agent_picker::AgentPicker,
    connect::ConnectPanel,
    llm_picker::LlmPicker,
    prompt,
    question::{self, QuestionPanel},
    session_picker::SessionPicker,
    settings::{self, SettingsPanel},
};

#[derive(Debug)]
pub(super) enum Input {
    Prompt,
    LlmPicker(LlmPicker),
    SessionPicker(SessionPicker),
    AgentPicker(AgentPicker),
    Question(QuestionPanel),
    Settings(SettingsPanel),
    Connect(ConnectPanel),
}

impl Input {
    /// Fixed while the input is open, so typing or filtering never moves
    /// the layout; the content panel takes up the difference when inputs
    /// swap. The question panel sizes itself to its questions, within
    /// `screen`.
    pub(super) fn rows(&self, screen: Rect) -> u16 {
        match self {
            Input::Prompt => prompt::ROWS,
            Input::LlmPicker(_) | Input::SessionPicker(_) | Input::AgentPicker(_) => {
                picker_rows(screen)
            }
            Input::Question(panel) => question::rows(panel, screen),
            Input::Settings(_) => settings::ROWS,
            Input::Connect(panel) => panel.rows(),
        }
    }
}

/// A picker's height: the title, the query and up to 14 items, but never
/// more than half the screen, so the chat stays in view.
fn picker_rows(screen: Rect) -> u16 {
    16.min(screen.height / 2).max(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pickers_grow_up_to_half_the_screen() {
        let screen = |height| Rect::new(0, 0, 80, height);
        assert_eq!(picker_rows(screen(60)), 16);
        assert_eq!(picker_rows(screen(20)), 10);
        assert_eq!(picker_rows(screen(6)), 4);
    }
}
