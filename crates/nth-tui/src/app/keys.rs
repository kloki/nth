//! The keymap: which key does what, kept apart from what doing it means.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, Completion, input::Input};
use crate::{command::Entry, mention, popup::Popup};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Submit,
    Interrupt,
    /// Highlights the next or previous entry in the completion popup.
    SelectNext,
    SelectPrev,
    /// Fills the prompt with the highlighted entry.
    Accept,
    /// Clears a non-empty prompt; quits on an empty one.
    ClearOrQuit,
    /// Opens the LLM picker, or closes any picker.
    LlmPicker,
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
        KeyCode::Char('n') if ctrl => Action::Accept,
        // Only told apart from Enter when the terminal disambiguates escape
        // codes; elsewhere ctrl+m is Enter and `/models` opens the picker.
        KeyCode::Char('m') if ctrl => Action::LlmPicker,
        KeyCode::Char(c) if !ctrl => Action::Insert(c),
        KeyCode::Esc => Action::Interrupt,
        KeyCode::Enter if ctrl => Action::Newline,
        KeyCode::Enter => Action::Submit,
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
        if let Input::LlmPicker(picker) = &mut self.input {
            match action {
                Action::SelectNext => picker.next(),
                Action::SelectPrev => picker.prev(),
                Action::Right => picker.more(),
                Action::Left => picker.less(),
                Action::Submit => self.choose_llm(),
                // Closing the picker must not also interrupt a running turn.
                Action::Interrupt | Action::ClearOrQuit | Action::LlmPicker => {
                    self.input = Input::Prompt
                }
                Action::PageUp => self.chat.page_up(),
                Action::PageDown => self.chat.page_down(),
                Action::Top => self.chat.jump_top(),
                Action::Bottom => self.chat.jump_bottom(),
                _ => {}
            }
            return;
        }
        if let Input::SessionPicker(picker) = &mut self.input {
            match action {
                Action::SelectNext => picker.next(),
                Action::SelectPrev => picker.prev(),
                Action::Submit => self.choose_session(),
                Action::Interrupt | Action::ClearOrQuit | Action::LlmPicker => {
                    self.input = Input::Prompt
                }
                Action::PageUp => self.chat.page_up(),
                Action::PageDown => self.chat.page_down(),
                Action::Top => self.chat.jump_top(),
                Action::Bottom => self.chat.jump_bottom(),
                _ => {}
            }
            return;
        }
        if let Some(completion) = &mut self.completion {
            match action {
                Action::SelectNext => return completion.next(),
                Action::SelectPrev => return completion.prev(),
                Action::Interrupt => {
                    self.completion = None;
                    return;
                }
                Action::Accept | Action::Submit => {
                    if let Some(completion) = self.completion.take() {
                        return self.accept(completion, action == Action::Submit);
                    }
                }
                _ => {}
            }
        }
        match action {
            Action::SelectNext | Action::SelectPrev | Action::Accept => {}
            Action::LlmPicker => self.open_llm_picker(),
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
        let text = self.prompt.text();
        self.completion = Entry::complete(text, &self.context.skills)
            .map(Completion::Command)
            .or_else(|| {
                let mention = mention::find(text, self.prompt.cursor())?;
                let files = mention::matches(&self.files, mention.query, mention::LIMIT);
                Some(Completion::File {
                    popup: Popup::new(files)?,
                    start: mention.start,
                })
            });
    }

    /// `submit` runs a highlighted command; a file is filled in either way,
    /// since sending a half-typed mention is never what Enter meant. A
    /// skill is filled in with room for its arguments, and runs once its
    /// name is typed out.
    fn accept(&mut self, completion: Completion, submit: bool) {
        match completion {
            Completion::Command(popup) => match popup.selected().clone() {
                Entry::Builtin(command) if submit => {
                    self.prompt.clear();
                    self.run_command(command);
                }
                Entry::Builtin(command) => {
                    self.prompt.set(&format!("/{}", command.name()));
                    self.completion = Some(Completion::Command(popup));
                }
                Entry::Skill { name, .. } if submit && self.prompt.text() == format!("/{name}") => {
                    self.submit()
                }
                Entry::Skill { name, .. } => self.prompt.set(&format!("/{name} ")),
            },
            Completion::File { popup, start } => {
                let end = self.prompt.cursor();
                let spaced = self.prompt.text()[end..].starts_with(char::is_whitespace);
                let gap = if spaced { "" } else { " " };
                self.prompt
                    .replace(start..end, &format!("@{}{gap}", popup.selected()));
            }
        }
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
        assert_eq!(key(KeyCode::Char('n'), ctrl), Some(Action::Accept));
        assert_eq!(key(KeyCode::Char('m'), ctrl), Some(Action::LlmPicker));
        assert_eq!(key(KeyCode::Down, none), Some(Action::SelectNext));
        assert_eq!(key(KeyCode::Up, none), Some(Action::SelectPrev));
        assert_eq!(key(KeyCode::Tab, none), None);
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
        match &app.completion {
            Some(Completion::Command(popup)) => match popup.selected() {
                Entry::Builtin(command) => *command,
                skill => panic!("{skill:?} is not a command"),
            },
            _ => panic!("command popup not open"),
        }
    }

    fn names(app: &App) -> Vec<&str> {
        match &app.completion {
            Some(Completion::Command(popup)) => popup.items().iter().map(Entry::name).collect(),
            _ => panic!("command popup not open"),
        }
    }

    /// An app whose project has the `fix` skill, with `text` typed.
    fn skilled(dir: &std::path::Path, text: &str) -> App {
        let mut app = app();
        app.context = crate::app::tests::with_fix_skill(dir);
        for c in text.chars() {
            app.apply(Action::Insert(c));
        }
        app
    }

    fn with_files(text: &str) -> App {
        let mut app = app();
        app.files = vec!["crates/nth-tui/src/app/keys.rs".into(), "README.md".into()];
        for c in text.chars() {
            app.apply(Action::Insert(c));
        }
        app
    }

    fn file(app: &App) -> &str {
        match &app.completion {
            Some(Completion::File { popup, .. }) => popup.selected(),
            _ => panic!("file popup not open"),
        }
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
        app.apply(Action::SelectNext);
        app.apply(Action::SelectNext);
        assert_eq!(selected(&app), Command::Clear);
        app.apply(Action::SelectPrev);
        assert_eq!(selected(&app), Command::Resume);
        assert_eq!(app.prompt.text(), "/");
    }

    #[test]
    fn ctrl_n_fills_in_the_highlighted_command() {
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
    fn skills_are_listed_after_the_commands() {
        let dir = tempfile::tempdir().expect("tempdir");

        let app = skilled(dir.path(), "/");
        assert_eq!(names(&app), ["clear", "exit", "models", "resume", "fix"]);

        let app = skilled(dir.path(), "/f");
        assert_eq!(names(&app), ["fix"]);
    }

    #[test]
    fn a_skill_is_filled_in_ready_for_its_arguments() {
        let dir = tempfile::tempdir().expect("tempdir");

        let mut app = skilled(dir.path(), "/f");
        app.apply(Action::Accept);
        assert_eq!(app.prompt.text(), "/fix ");
        assert!(app.completion.is_none());

        let mut app = skilled(dir.path(), "/f");
        app.apply(Action::Submit);
        assert_eq!(
            app.prompt.text(),
            "/fix ",
            "Enter fills in a half-typed name"
        );
        assert!(!app.is_busy());
    }

    #[tokio::test]
    async fn enter_on_a_typed_out_skill_runs_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut app = skilled(dir.path(), "/fix");

        app.apply(Action::Submit);

        assert!(app.is_busy());
        assert!(app.prompt.is_empty());
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
    fn at_opens_the_file_popup_anywhere_in_the_prompt() {
        let mut app = with_files("see @");
        assert_eq!(file(&app), "README.md");

        app.apply(Action::Insert('k'));
        assert_eq!(file(&app), "crates/nth-tui/src/app/keys.rs");

        app.apply(Action::Insert(' '));
        assert!(app.completion.is_none());
    }

    #[test]
    fn ctrl_n_and_enter_fill_in_the_file() {
        let mut app = with_files("see @ke");
        app.apply(Action::Accept);
        assert_eq!(app.prompt.text(), "see @crates/nth-tui/src/app/keys.rs ");
        assert!(app.completion.is_none());

        for c in "and @rea".chars() {
            app.apply(Action::Insert(c));
        }
        app.apply(Action::Submit);
        assert_eq!(
            app.prompt.text(),
            "see @crates/nth-tui/src/app/keys.rs and @README.md "
        );
        assert!(!app.is_busy());
    }

    #[test]
    fn a_mention_before_the_cursor_is_completed_in_place() {
        let mut app = with_files("@rea tail");
        for _ in 0.." tail".len() {
            app.apply(Action::Left);
        }
        app.apply(Action::Backspace);
        app.apply(Action::Insert('a'));
        app.apply(Action::Accept);
        assert_eq!(app.prompt.text(), "@README.md tail");
    }

    #[test]
    fn esc_closes_the_file_popup() {
        let mut app = with_files("@");
        app.apply(Action::Interrupt);
        assert!(app.completion.is_none());
        assert!(!app.is_busy());
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
