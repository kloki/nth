//! The keymap: which key does what, kept apart from what doing it means.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nth_protocol::Reply;

use super::{App, input::Input};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    Submit,
    Interrupt,
    /// Highlights the next or previous entry in the completion popup or a
    /// picker; on the prompt, recalls the next or previous sent prompt.
    SelectNext,
    SelectPrev,
    /// Fills the prompt with the highlighted entry.
    Accept,
    /// Clears a non-empty prompt; quits on an empty one.
    ClearOrQuit,
    /// Opens the LLM picker, or closes any picker.
    LlmPicker,
    Insert(char),
    /// Moves between the question panel's tabs.
    NextTab,
    PrevTab,
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
    /// Shows the content panel's next tab.
    NextContent,
    /// Shows the content panel's tab at this index, from 0.
    Content(usize),
    /// Closes the content panel's tab showing, unless it is the chat or a
    /// monitor still running.
    CloseContent,
    /// Stops the monitor whose tab is showing, or closes its tab once it
    /// has stopped.
    StopContent,
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
        KeyCode::Char('t') if ctrl => Action::NextContent,
        KeyCode::Char('q') if ctrl => Action::CloseContent,
        KeyCode::Char('w') if ctrl => Action::StopContent,
        // Like ctrl+m, only told apart from the bare digit where the
        // terminal disambiguates escape codes; ctrl+t reaches every tab.
        KeyCode::Char(c @ '1'..='4') if ctrl => Action::Content(usize::from(c as u8 - b'1')),
        KeyCode::Char(c) if !ctrl => Action::Insert(c),
        KeyCode::Esc => Action::Interrupt,
        KeyCode::Enter if ctrl => Action::Newline,
        KeyCode::Enter => Action::Submit,
        KeyCode::Tab => Action::NextTab,
        KeyCode::BackTab => Action::PrevTab,
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
        // A hint is for the key that caused it; quitting needs ctrl+c twice
        // in a row.
        self.hint = None;
        if action != Action::ClearOrQuit {
            self.quit_armed = false;
        }
        // Tabs switch and scroll whichever input panel is open, as it
        // never changes with them.
        match action {
            Action::NextContent => return self.content.next(),
            Action::Content(index) => return self.content.select(index),
            Action::CloseContent => return self.close_content(),
            Action::StopContent => return self.stop_content(),
            Action::PageUp | Action::PageDown | Action::Top | Action::Bottom => {
                return self.scroll(action);
            }
            _ => {}
        }
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
                _ => {}
            }
            return;
        }
        if let Input::Question(panel) = &mut self.input {
            let editing = panel.editing();
            let answers = match action {
                Action::SelectNext => {
                    panel.next();
                    None
                }
                Action::SelectPrev => {
                    panel.prev();
                    None
                }
                Action::NextTab => {
                    panel.next_tab();
                    None
                }
                Action::PrevTab => {
                    panel.prev_tab();
                    None
                }
                Action::Insert(c) => panel.insert(c),
                Action::Submit => panel.enter(),
                // Esc declines the questions; the turn runs on, and a
                // second Esc at the prompt cancels it as usual.
                Action::Interrupt | Action::ClearOrQuit => {
                    self.reply(Reply::Declined);
                    return;
                }
                // ←→ edit the open field while it is highlighted, and move
                // between questions anywhere else.
                Action::Right if !editing => {
                    panel.next_tab();
                    None
                }
                Action::Left if !editing => {
                    panel.prev_tab();
                    None
                }
                action => {
                    if let Some(open) = panel.open_mut() {
                        match action {
                            Action::Backspace => open.backspace(),
                            Action::Delete => open.delete(),
                            Action::Left => open.left(),
                            Action::Right => open.right(),
                            Action::LineStart => open.home(),
                            Action::LineEnd => open.end(),
                            _ => {}
                        }
                    }
                    None
                }
            };
            if let Some(answers) = answers {
                self.reply(Reply::Answered(answers));
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
            Action::SelectPrev => {
                if let Some(text) = self.history.prev(self.prompt.text()) {
                    self.prompt.set(&text);
                }
            }
            Action::SelectNext => {
                if let Some(text) = self.history.next(self.prompt.text()) {
                    self.prompt.set(&text);
                }
            }
            Action::Accept | Action::NextTab | Action::PrevTab => {}
            // Taken before any input panel sees them.
            Action::NextContent
            | Action::Content(_)
            | Action::CloseContent
            | Action::StopContent
            | Action::PageUp
            | Action::PageDown
            | Action::Top
            | Action::Bottom => {}
            Action::LlmPicker => self.open_llm_picker(),
            Action::Submit => self.submit(),
            Action::Interrupt => self.interrupt(),
            Action::ClearOrQuit if self.prompt.is_empty() => self.ask_quit(),
            Action::ClearOrQuit => self.prompt.clear(),
            Action::Insert(c) => self.prompt.insert(c),
            Action::Newline => self.prompt.insert('\n'),
            Action::Backspace => self.prompt.backspace(),
            Action::Delete => self.prompt.delete(),
            Action::Left => self.prompt.left(),
            Action::Right => self.prompt.right(),
            Action::LineStart => self.prompt.home(),
            Action::LineEnd => self.prompt.end(),
        }
        match action {
            Action::Insert(_) | Action::Newline | Action::Backspace | Action::Delete => {
                self.refresh_completion()
            }
            // Anything else on the prompt closes the popup; scrolling
            // returned before reaching here, so it leaves the popup be.
            _ => self.completion = None,
        }
    }

    /// Scrolls whichever tab is showing.
    fn scroll(&mut self, action: Action) {
        let Some(view) = self.active_view() else {
            return;
        };
        match action {
            Action::PageUp => view.page_up(),
            Action::PageDown => view.page_down(),
            Action::Top => view.jump_top(),
            Action::Bottom => view.jump_bottom(),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{completion::Completion, tests::app},
        command::{Command, Entry},
    };

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
        assert_eq!(key(KeyCode::Tab, none), Some(Action::NextTab));
        assert_eq!(key(KeyCode::BackTab, none), Some(Action::PrevTab));
        assert_eq!(key(KeyCode::Char('t'), ctrl), Some(Action::NextContent));
        assert_eq!(key(KeyCode::Char('q'), ctrl), Some(Action::CloseContent));
        assert_eq!(key(KeyCode::Char('1'), ctrl), Some(Action::Content(0)));
        assert_eq!(key(KeyCode::Char('4'), ctrl), Some(Action::Content(3)));
        assert_eq!(key(KeyCode::Char('5'), ctrl), None);
        assert_eq!(key(KeyCode::Char('1'), none), Some(Action::Insert('1')));
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
        assert_eq!(selected(&app), Command::Close);
        for _ in 0..5 {
            app.apply(Action::SelectNext);
        }
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
        assert_eq!(app.prompt.text(), "/close");
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
        assert_eq!(
            names(&app),
            [
                "clear",
                "close",
                "diagnostics",
                "exit",
                "models",
                "resume",
                "fix"
            ]
        );

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

    /// An app that sent `sent`, oldest first, with `draft` typed.
    fn recalling(sent: &[&str], draft: &str) -> App {
        let mut app = app();
        for prompt in sent {
            app.history.push(prompt.to_string());
        }
        app.prompt.insert_str(draft);
        app
    }

    #[test]
    fn up_and_down_recall_sent_prompts_and_give_the_draft_back() {
        let mut app = recalling(&["first", "second"], "draft");

        app.apply(Action::SelectPrev);
        assert_eq!(app.prompt.text(), "second");
        assert_eq!(app.prompt.cursor(), "second".len());
        app.apply(Action::SelectPrev);
        assert_eq!(app.prompt.text(), "first");
        app.apply(Action::SelectPrev);
        assert_eq!(app.prompt.text(), "first", "stops at the oldest");

        app.apply(Action::SelectNext);
        app.apply(Action::SelectNext);
        assert_eq!(app.prompt.text(), "draft");
    }

    #[test]
    fn up_leaves_an_edited_prompt_alone() {
        let mut app = recalling(&["first", "second"], "");
        app.apply(Action::SelectPrev);
        app.apply(Action::Insert('!'));

        app.apply(Action::SelectPrev);
        assert_eq!(app.prompt.text(), "second!");
    }

    #[test]
    fn up_moves_an_open_popup_rather_than_recalling() {
        let mut app = recalling(&["sent"], "");
        for c in "/".chars() {
            app.apply(Action::Insert(c));
        }
        app.apply(Action::SelectPrev);
        assert_eq!(selected(&app), Command::Resume);
        assert_eq!(app.prompt.text(), "/");
    }

    #[test]
    fn a_recalled_command_does_not_open_the_popup() {
        let mut app = recalling(&["/models"], "");
        app.apply(Action::SelectPrev);
        assert_eq!(app.prompt.text(), "/models");
        assert!(app.completion.is_none());
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
