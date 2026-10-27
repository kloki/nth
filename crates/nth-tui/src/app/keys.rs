//! The keymap: which key does what, kept apart from what doing it means.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{App, input::Input};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    CopyCurio,
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
    /// Cycles to the next profile, both modes on its model and effort; a
    /// model picked by hand is put back on the current profile first.
    NextProfile,
    /// Opens the showing view in your editor: the plan on its tab, else
    /// the prompt.
    Edit,
    Insert(char),
    /// Moves between the question panel's tabs; at the prompt, switches
    /// between plan and act.
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
    /// Shows the content panel's previous tab.
    PrevContent,
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
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let action = match key.code {
        KeyCode::Char('c') if ctrl => Action::ClearOrQuit,
        KeyCode::Char('j') if ctrl => Action::Newline,
        KeyCode::Char('u') if ctrl => Action::PageUp,
        KeyCode::Char('d') if ctrl => Action::PageDown,
        KeyCode::Char('n') if ctrl => Action::Accept,
        // Only told apart from Enter when the terminal disambiguates escape
        // codes; elsewhere ctrl+m is Enter and `/models` opens the picker.
        KeyCode::Char('m') if ctrl => Action::LlmPicker,
        KeyCode::Char('p') if ctrl => Action::NextProfile,
        KeyCode::Char('g') if ctrl => Action::Edit,
        KeyCode::Char('y') if ctrl => Action::CopyCurio,
        // Terminals report ctrl+shift+t either way; only those that
        // disambiguate escape codes tell it from ctrl+t at all.
        KeyCode::Char('T') if ctrl => Action::PrevContent,
        KeyCode::Char('t') if ctrl && shift => Action::PrevContent,
        KeyCode::Char('t') if ctrl => Action::NextContent,
        KeyCode::Char('q') if ctrl => Action::CloseContent,
        KeyCode::Char('w') if ctrl => Action::StopContent,
        // Like ctrl+m, only told apart from the bare digit where the
        // terminal disambiguates escape codes; ctrl+t reaches every tab.
        KeyCode::Char(c @ '1'..='4') if ctrl => Action::Content(usize::from(c as u8 - b'1')),
        KeyCode::Char(c) if !ctrl => Action::Insert(c),
        KeyCode::Esc => Action::Interrupt,
        // Only told apart from Enter where the terminal disambiguates escape
        // codes; ctrl+j breaks a line everywhere.
        KeyCode::Enter if ctrl || shift => Action::Newline,
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
            Action::PrevContent => return self.content.prev(),
            Action::Content(index) => return self.content.select(index),
            Action::CloseContent => return self.close_content(),
            Action::StopContent => return self.stop_content(),
            Action::PageUp | Action::PageDown | Action::Top | Action::Bottom => {
                return self.scroll(action);
            }
            _ => {}
        }
        match &self.input {
            Input::Curios(_) => return self.key_curios(action),
            Input::LlmPicker(_) => return self.key_llm_picker(action),
            Input::SessionPicker(_) => return self.key_session_picker(action),
            Input::AgentPicker(_) => return self.key_agent_picker(action),
            Input::Settings(_) => return self.key_settings(action),
            Input::Connect(_) => return self.key_connect(action),
            Input::Question(_) => return self.key_question(action),
            Input::Prompt => {}
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
                if let Some(entry) = self.history.prev(&self.prompt.entry()) {
                    self.prompt.set_entry(&entry);
                }
            }
            Action::SelectNext => {
                if let Some(entry) = self.history.next(&self.prompt.entry()) {
                    self.prompt.set_entry(&entry);
                }
            }
            Action::Accept => {}
            // A command runs without the model, so neither mode applies;
            // nor does a subagent, whose tab the prompt talks to.
            Action::NextTab | Action::PrevTab
                if self.prompt.shell() || self.showing_subagent().is_some() => {}
            Action::NextTab | Action::PrevTab => self.toggle_mode(),
            // Taken before any input panel sees them.
            Action::NextContent
            | Action::PrevContent
            | Action::Content(_)
            | Action::CloseContent
            | Action::StopContent
            | Action::PageUp
            | Action::PageDown
            | Action::Top
            | Action::Bottom => {}
            Action::LlmPicker => self.open_llm_picker(),
            Action::NextProfile => self.next_profile(),
            Action::Edit => self.edit(),
            Action::Submit => self.submit(),
            // Leaving command mode comes before cancelling the turn.
            Action::Interrupt if self.prompt.shell() => self.prompt.set_shell(false),
            // On a subagent's tab, Esc is for it, never for the parent.
            Action::Interrupt => match self.showing_subagent() {
                Some(id) => self.interrupt_subagent(id),
                None => self.interrupt(),
            },
            Action::ClearOrQuit if self.prompt.is_empty() && self.prompt.shell() => {
                self.prompt.set_shell(false)
            }
            Action::ClearOrQuit if self.prompt.is_empty() => self.ask_quit(),
            Action::ClearOrQuit => self.prompt.clear(),
            // `!` at the start makes the prompt a command to run, as in
            // opencode; anywhere else it is just text.
            Action::Insert('!')
                if self.prompt.cursor() == 0
                    && !self.prompt.shell()
                    && self.completion.is_none() =>
            {
                self.prompt.set_shell(true)
            }
            Action::Insert(c) => self.prompt.insert(c),
            Action::Newline => self.prompt.insert('\n'),
            Action::Backspace if self.prompt.cursor() == 0 && self.prompt.shell() => {
                self.prompt.set_shell(false)
            }
            Action::Backspace => self.prompt.backspace(),
            Action::Delete => self.prompt.delete(),
            Action::Left => self.prompt.left(),
            Action::Right => self.prompt.right(),
            Action::LineStart => self.prompt.home(),
            Action::LineEnd => self.prompt.end(),
            Action::CopyCurio => {}
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
mod tests;
