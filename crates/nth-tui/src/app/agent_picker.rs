//! `/agents`: picking what later turns run as, a mode or a primary agent.

use super::{App, input::Input, keys::Action};
use crate::agent_picker::{AgentPicker, Choice};

impl App {
    /// A key while the agent picker is open.
    pub(super) fn key_agent_picker(&mut self, action: Action) {
        let Input::AgentPicker(picker) = &mut self.input else {
            return;
        };
        match action {
            Action::SelectNext => picker.next(),
            Action::SelectPrev => picker.prev(),
            Action::Insert(c) => picker.insert(c),
            Action::Backspace => picker.backspace(),
            Action::Submit => self.choose_agent(),
            Action::ClearOrQuit if picker.clear_query() => {}
            Action::Interrupt | Action::ClearOrQuit => self.input = Input::Prompt,
            _ => {}
        }
    }

    /// Opens the picker on what runs now.
    pub(super) fn open_agent_picker(&mut self) {
        self.completion = None;
        let current = match &self.agent {
            Some(name) => Choice::Agent(name.clone()),
            None => Choice::Mode(self.mode),
        };
        self.input = Input::AgentPicker(AgentPicker::new(&self.context.agents, current));
    }

    /// Runs later turns as the highlighted mode or agent; one running now
    /// keeps what it runs as.
    fn choose_agent(&mut self) {
        let Input::AgentPicker(picker) = &self.input else {
            return;
        };
        let Some(choice) = picker.chosen() else {
            return;
        };
        self.input = Input::Prompt;
        match choice {
            Choice::Mode(mode) => self.set_mode(mode),
            Choice::Agent(name) => self.set_agent(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::Mode;

    use super::*;
    use crate::app::tests::{app, rows};

    #[test]
    fn picking_an_agent_runs_it_until_tab_goes_back_to_the_mode() {
        let mut app = app();
        app.mode = Mode::Plan;
        app.run_command(crate::command::Command::Agents);
        assert!(matches!(app.input, Input::AgentPicker(_)));
        let shown = rows(&mut app).join("\n");
        assert!(shown.contains("run as"), "{shown}");

        // plan, act, then no primary agents in a bare app: down to act.
        app.apply(Action::SelectNext);
        app.apply(Action::Submit);
        assert_eq!((app.mode, app.agent.as_deref()), (Mode::Act, None));
        assert!(matches!(app.input, Input::Prompt));

        app.set_agent("review".into());
        assert_eq!(app.agent.as_deref(), Some("review"));
        let label = rows(&mut app)
            .into_iter()
            .find(|row| row.contains("▎ review"))
            .expect("the agent labels the prompt");
        assert!(label.contains("· "), "{label}");

        app.apply(Action::NextTab);
        assert_eq!(
            (app.mode, app.agent.as_deref()),
            (Mode::Act, None),
            "Tab leaves the agent for the mode it left"
        );
        app.apply(Action::NextTab);
        assert_eq!(app.mode, Mode::Plan, "then switches modes as ever");
    }
}
