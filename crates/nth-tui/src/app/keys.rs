//! The keymap: which key does what, kept apart from what doing it means.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::App;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Submit,
    Interrupt,
    /// Clears a non-empty prompt; quits on an empty one.
    ClearOrQuit,
    Insert(char),
    Newline,
    Backspace,
    Delete,
    Left,
    Right,
    LineStart,
    LineEnd,
    PageUp,
    PageDown,
    Top,
    Bottom,
}

pub fn action(key: KeyEvent) -> Option<Action> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let action = match key.code {
        KeyCode::Char('c') if ctrl => Action::ClearOrQuit,
        KeyCode::Char('j') if ctrl => Action::Newline,
        KeyCode::Char('u') if ctrl => Action::PageUp,
        KeyCode::Char('d') if ctrl => Action::PageDown,
        KeyCode::Char(c) if !ctrl => Action::Insert(c),
        KeyCode::Esc => Action::Interrupt,
        KeyCode::Enter if ctrl => Action::Newline,
        KeyCode::Enter => Action::Submit,
        KeyCode::PageUp => Action::PageUp,
        KeyCode::PageDown => Action::PageDown,
        KeyCode::Home if ctrl => Action::Top,
        KeyCode::End if ctrl => Action::Bottom,
        KeyCode::Home => Action::LineStart,
        KeyCode::End => Action::LineEnd,
        KeyCode::Left => Action::Left,
        KeyCode::Right => Action::Right,
        KeyCode::Backspace => Action::Backspace,
        KeyCode::Delete => Action::Delete,
        _ => return None,
    };
    Some(action)
}

impl App {
    pub(super) fn on_key(&mut self, key: KeyEvent) {
        if let Some(action) = action(key) {
            self.apply(action);
        }
    }

    pub(super) fn apply(&mut self, action: Action) {
        match action {
            Action::Submit => self.submit(),
            Action::Interrupt => self.interrupt(),
            Action::ClearOrQuit if self.prompt.is_empty() => self.quit = true,
            Action::ClearOrQuit => self.prompt.clear(),
            Action::Insert(c) => self.prompt.insert(c),
            Action::Newline => self.prompt.insert('\n'),
            Action::Backspace => self.prompt.backspace(),
            Action::Delete => self.prompt.delete(),
            Action::Left => self.prompt.left(),
            Action::Right => self.prompt.right(),
            Action::LineStart => self.prompt.home(),
            Action::LineEnd => self.prompt.end(),
            Action::PageUp => self.chat.page_up(),
            Action::PageDown => self.chat.page_down(),
            Action::Top => self.chat.jump_top(),
            Action::Bottom => self.chat.jump_bottom(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::tests::app;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        action(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn maps_keys_to_actions() {
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;

        assert_eq!(key(KeyCode::Esc, none), Some(Action::Interrupt));
        assert_eq!(key(KeyCode::Enter, none), Some(Action::Submit));
        assert_eq!(
            key(KeyCode::Enter, KeyModifiers::SHIFT),
            Some(Action::Newline)
        );
        assert_eq!(key(KeyCode::Char('j'), ctrl), Some(Action::Newline));
        assert_eq!(key(KeyCode::Char('c'), ctrl), Some(Action::ClearOrQuit));
        assert_eq!(key(KeyCode::Char('c'), none), Some(Action::Insert('c')));
        assert_eq!(key(KeyCode::Home, ctrl), Some(Action::Top));
        assert_eq!(key(KeyCode::Home, none), Some(Action::LineStart));
        assert_eq!(key(KeyCode::Char('x'), ctrl), None);
    }

    #[test]
    fn ctrl_c_clears_the_prompt_before_quitting() {
        let mut app = app();
        app.prompt.insert_str("draft");

        app.apply(Action::ClearOrQuit);
        assert!(app.prompt.is_empty());
        assert!(!app.quit);

        app.apply(Action::ClearOrQuit);
        assert!(app.quit);
    }
}
