//! The keymap: which key does what, kept apart from what doing it means.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::App;
use crate::command::Completion;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Submit,
    Interrupt,
    /// Highlights the next or previous command in the completion popup.
    SelectNext,
    SelectPrev,
    /// Fills the prompt with the highlighted command.
    Accept,
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
        KeyCode::Char('n') if ctrl => Action::SelectNext,
        KeyCode::Char('p') if ctrl => Action::SelectPrev,
        KeyCode::Char(c) if !ctrl => Action::Insert(c),
        KeyCode::Esc => Action::Interrupt,
        KeyCode::Enter if ctrl => Action::Newline,
        KeyCode::Enter => Action::Submit,
        KeyCode::Tab => Action::Accept,
        KeyCode::Down => Action::SelectNext,
        KeyCode::Up => Action::SelectPrev,
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
        if let Some(completion) = &mut self.completion {
            match action {
                Action::SelectNext => return completion.next(),
                Action::SelectPrev => return completion.prev(),
                Action::Accept => {
                    let name = format!("/{}", completion.selected().name());
                    return self.prompt.set(&name);
                }
                Action::Submit => {
                    let command = completion.selected();
                    self.completion = None;
                    self.prompt.clear();
                    return self.run_command(command);
                }
                Action::Interrupt => {
                    self.completion = None;
                    return;
                }
                _ => {}
            }
        }
        match action {
            Action::SelectNext | Action::SelectPrev | Action::Accept => {}
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
        match action {
            Action::Insert(_) | Action::Newline | Action::Backspace | Action::Delete => {
                self.refresh_completion()
            }
            // Chat scrolling leaves the popup be; anything else on the
            // prompt closes it.
            Action::PageUp | Action::PageDown | Action::Top | Action::Bottom => {}
            _ => self.completion = None,
        }
    }

    pub(super) fn refresh_completion(&mut self) {
        self.completion = Completion::new(self.prompt.text());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{app::tests::app, command::Command};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> Option<Action> {
        action(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn maps_keys_to_actions() {
        let none = KeyModifiers::NONE;
        let ctrl = KeyModifiers::CONTROL;

        assert_eq!(key(KeyCode::Esc, none), Some(Action::Interrupt));
        assert_eq!(key(KeyCode::Enter, none), Some(Action::Submit));
        assert_eq!(key(KeyCode::Enter, ctrl), Some(Action::Newline));
        assert_eq!(key(KeyCode::Char('j'), ctrl), Some(Action::Newline));
        assert_eq!(key(KeyCode::Char('c'), ctrl), Some(Action::ClearOrQuit));
        assert_eq!(key(KeyCode::Char('c'), none), Some(Action::Insert('c')));
        assert_eq!(key(KeyCode::Home, ctrl), Some(Action::Top));
        assert_eq!(key(KeyCode::Home, none), Some(Action::LineStart));
        assert_eq!(key(KeyCode::Char('n'), ctrl), Some(Action::SelectNext));
        assert_eq!(key(KeyCode::Char('p'), ctrl), Some(Action::SelectPrev));
        assert_eq!(key(KeyCode::Down, none), Some(Action::SelectNext));
        assert_eq!(key(KeyCode::Up, none), Some(Action::SelectPrev));
        assert_eq!(key(KeyCode::Tab, none), Some(Action::Accept));
        assert_eq!(key(KeyCode::Char('x'), ctrl), None);
    }

    fn typed(text: &str) -> App {
        let mut app = app();
        for c in text.chars() {
            app.apply(Action::Insert(c));
        }
        app
    }

    fn selected(app: &App) -> Command {
        app.completion.as_ref().expect("popup open").selected()
    }

    #[test]
    fn slash_opens_the_popup_and_typing_filters_it() {
        let mut app = typed("/");
        assert_eq!(selected(&app), Command::Clear);

        app.apply(Action::Insert('e'));
        assert_eq!(selected(&app), Command::Exit);

        app.apply(Action::Insert('x'));
        app.apply(Action::Insert('x'));
        assert!(app.completion.is_none());
    }

    #[test]
    fn arrows_cycle_without_touching_the_prompt() {
        let mut app = typed("/");
        app.apply(Action::SelectNext);
        assert_eq!(selected(&app), Command::Exit);
        app.apply(Action::SelectNext);
        assert_eq!(selected(&app), Command::Clear);
        app.apply(Action::SelectPrev);
        assert_eq!(selected(&app), Command::Exit);
        assert_eq!(app.prompt.text(), "/");
    }

    #[test]
    fn tab_fills_in_the_highlighted_command() {
        let mut app = typed("/");
        app.apply(Action::SelectNext);
        app.apply(Action::Accept);
        assert_eq!(app.prompt.text(), "/exit");
        assert!(app.completion.is_some());
    }

    #[test]
    fn enter_runs_the_highlighted_command() {
        let mut app = typed("/ex");
        app.apply(Action::Submit);
        assert!(app.quit);
        assert!(app.prompt.is_empty());
        assert!(app.completion.is_none());
    }

    #[test]
    fn esc_closes_the_popup_until_the_next_edit() {
        let mut app = typed("/c");
        app.apply(Action::Interrupt);
        assert!(app.completion.is_none());

        app.apply(Action::SelectNext);
        assert!(app.completion.is_none());

        app.apply(Action::Backspace);
        assert!(app.completion.is_some());
    }

    #[test]
    fn plain_prompts_never_open_the_popup() {
        let mut app = typed("hello");
        assert!(app.completion.is_none());
        app.apply(Action::Accept);
        assert_eq!(app.prompt.text(), "hello");
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
