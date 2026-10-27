//! Plan and act: Tab switches between them, and each keeps its own model
//! and effort, so planning can run on a different model than acting.
//! Subagents have a third, which by default is the parent's. A primary
//! agent picked with `/agents` runs in place of the mode, on a model of its
//! own for this run, until Tab goes back to the mode.

pub use nth_protocol::Llm;
use nth_protocol::Mode;

use super::App;
use crate::llm_picker::Target;

/// What each mode runs with, and what the subagents its turns start do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeLlms {
    pub plan: Llm,
    pub act: Llm,
    /// `None` runs subagents on the model of the mode that starts them.
    pub subagent: Option<Llm>,
    /// `None` runs the built-in clerk on the subagents' model.
    pub clerk: Option<Llm>,
}

impl ModeLlms {
    /// Both modes on `llm`, subagents on theirs.
    pub fn same(llm: Llm) -> Self {
        Self {
            plan: llm.clone(),
            act: llm,
            subagent: None,
            clerk: None,
        }
    }

    fn get(&self, mode: Mode) -> &Llm {
        match mode {
            Mode::Plan => &self.plan,
            Mode::Act => &self.act,
        }
    }

    fn get_mut(&mut self, mode: Mode) -> &mut Llm {
        match mode {
            Mode::Plan => &mut self.plan,
            Mode::Act => &mut self.act,
        }
    }
}

/// What a turn runs as and on: the mode, the agent running in its place,
/// and the model and effort.
pub(super) type Steerable = (Mode, Option<String>, Llm);

impl App {
    /// What the next turn runs as and on.
    pub(super) fn runs_as(&self) -> Steerable {
        (self.mode, self.agent.clone(), self.current_llm())
    }

    /// What each mode runs with; the current mode's model and effort come
    /// from it too.
    pub fn with_mode_llms(mut self, llms: ModeLlms) -> Self {
        self.set_mode_llms(llms);
        self
    }

    /// Both modes' llms at once, and the current mode's model and effort
    /// with them: what applying a profile does.
    pub(super) fn set_mode_llms(&mut self, llms: ModeLlms) {
        self.mode_llms = llms;
        self.load_llm();
    }

    /// Switches to `mode`, and to the model and effort it runs with,
    /// leaving the agent that ran in its place. The next turn runs in it;
    /// one running now keeps its mode.
    pub(super) fn set_mode(&mut self, mode: Mode) {
        self.save_llm();
        self.agent = None;
        self.mode = mode;
        self.load_llm();
    }

    /// Tab: back to the mode an agent ran in place of, else to the other
    /// mode.
    pub(super) fn toggle_mode(&mut self) {
        match self.agent {
            Some(_) => self.set_mode(self.mode),
            None => self.set_mode(self.mode.toggled()),
        }
    }

    /// Runs later turns as the primary agent `name`, on what it ran on
    /// last in this run, else its own model, else act's.
    pub(super) fn set_agent(&mut self, name: String) {
        self.save_llm();
        self.agent = Some(name);
        self.load_llm();
    }

    /// Keeps the model and effort picked for what runs now, as the picker
    /// changes `model` and `effort` directly.
    pub(super) fn save_llm(&mut self) {
        let llm = self.current_llm();
        match &self.agent {
            Some(name) => {
                self.agent_llms.insert(name.clone(), llm);
            }
            None => *self.mode_llms.get_mut(self.mode) = llm,
        }
    }

    /// What the next turn runs with: the live pick, as the picker changes
    /// `model` and `effort` before it is saved.
    pub(super) fn current_llm(&self) -> Llm {
        Llm {
            model: self.model.clone(),
            effort: self.effort,
        }
    }

    /// What `mode` runs with: the current mode's live pick while no agent
    /// runs in its place.
    pub(super) fn llm(&self, mode: Mode) -> Llm {
        match mode == self.mode && self.agent.is_none() {
            true => self.current_llm(),
            false => self.mode_llms.get(mode).clone(),
        }
    }

    /// What the primary agent `name` runs with: what it ran on last in this
    /// run, else its own model at act's effort, else act's.
    fn agent_llm(&self, name: &str) -> Llm {
        if let Some(llm) = self.agent_llms.get(name) {
            return llm.clone();
        }
        let act = self.mode_llms.act.clone();
        match self
            .context
            .agents
            .primary(name)
            .and_then(|a| a.model.clone())
        {
            Some(model) => Llm {
                model,
                effort: act.effort,
            },
            None => act,
        }
    }

    /// What the subagents of the next turn run on: their own pick, else
    /// what the turn runs on.
    pub(super) fn subagent_llm(&self) -> Llm {
        self.mode_llms
            .subagent
            .clone()
            .unwrap_or_else(|| self.current_llm())
    }

    /// What the clerk of the next turn runs on: its own pick, else the
    /// subagents'.
    pub(super) fn clerk_llm(&self) -> Llm {
        self.mode_llms
            .clerk
            .clone()
            .unwrap_or_else(|| self.subagent_llm())
    }

    /// What `target` runs on now.
    pub(super) fn llm_of(&self, target: Target) -> Llm {
        match target {
            Target::Plan => self.llm(Mode::Plan),
            Target::Act => self.llm(Mode::Act),
            Target::Agent => self.current_llm(),
            Target::Subagent => self.subagent_llm(),
            Target::Clerk => self.clerk_llm(),
        }
    }

    /// Puts `target` on `llm`, from the next turn.
    pub(super) fn set_llm(&mut self, target: Target, llm: Llm) {
        let mode = match target {
            Target::Plan => Mode::Plan,
            Target::Act => Mode::Act,
            // Only offered while an agent runs.
            Target::Agent => {
                if let Some(name) = &self.agent {
                    self.agent_llms.insert(name.clone(), llm);
                    self.load_llm();
                }
                return;
            }
            Target::Subagent => {
                self.mode_llms.subagent = Some(llm);
                return;
            }
            Target::Clerk => {
                self.mode_llms.clerk = Some(llm);
                return;
            }
        };
        *self.mode_llms.get_mut(mode) = llm;
        if mode == self.mode && self.agent.is_none() {
            self.load_llm();
        }
    }

    fn load_llm(&mut self) {
        let llm = match &self.agent {
            Some(name) => self.agent_llm(name),
            None => self.mode_llms.get(self.mode).clone(),
        };
        self.model = llm.model;
        self.effort = llm.effort;
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::Effort;

    use super::*;
    use crate::app::{
        keys::Action,
        tests::{app, rows},
    };

    fn llm(model: &str, effort: Effort) -> Llm {
        Llm {
            model: model.into(),
            effort,
        }
    }

    #[test]
    fn tab_switches_the_mode_and_its_model() {
        let mut app = app().with_mode_llms(ModeLlms {
            plan: llm("kimi", Effort::High),
            act: llm("glm", Effort::Default),
            subagent: None,
            clerk: None,
        });
        assert_eq!(app.mode, Mode::Act, "a bare session acts");
        assert_eq!(
            crate::app::tests::prompt_row(&rows(&mut app), 0).trim_end(),
            " ▎ act  · glm"
        );

        app.apply(Action::NextTab);
        assert_eq!(app.mode, Mode::Plan);
        assert_eq!((app.model.as_str(), app.effort), ("kimi", Effort::High));
        let shown = rows(&mut app);
        assert_eq!(
            crate::app::tests::prompt_row(&shown, 0).trim_end(),
            " ▎ plan · kimi · high"
        );
        assert_eq!(
            crate::app::tests::status_row(&shown, 0).trim_end(),
            " /repo 0m"
        );
        assert_eq!(
            crate::app::tests::prompt_row(&shown, 0).find("· kimi"),
            " ▎ act  · glm".find("· glm"),
            "the model keeps its column across modes"
        );

        app.effort = Effort::Low;
        app.apply(Action::PrevTab);
        assert_eq!((app.mode, app.model.as_str()), (Mode::Act, "glm"));
        app.apply(Action::NextTab);
        assert_eq!(app.effort, Effort::Low, "the pick stays with its mode");
    }

    #[test]
    fn an_agent_keeps_its_own_llm_apart_from_the_modes() {
        let mut app = app().with_mode_llms(ModeLlms {
            plan: llm("kimi", Effort::High),
            act: llm("glm", Effort::Default),
            subagent: None,
            clerk: None,
        });
        app.set_mode(Mode::Plan);

        app.set_agent("review".into());
        assert_eq!(
            app.current_llm(),
            llm("glm", Effort::Default),
            "act's to start"
        );
        app.set_llm(Target::Agent, llm("qwen", Effort::Low));
        assert_eq!(app.model, "qwen");
        assert_eq!(
            app.llm(Mode::Act),
            llm("glm", Effort::Default),
            "act keeps its own"
        );

        app.apply(Action::NextTab);
        assert_eq!((app.mode, app.model.as_str()), (Mode::Plan, "kimi"));
        app.set_agent("review".into());
        assert_eq!(app.model, "qwen", "the agent's pick holds for the run");
        assert_eq!(app.runs_as().1.as_deref(), Some("review"));
    }

    #[test]
    fn each_mode_has_its_llm_while_one_is_picked() {
        let mut app = app().with_mode_llms(ModeLlms {
            plan: llm("kimi", Effort::High),
            act: llm("glm", Effort::Default),
            subagent: None,
            clerk: None,
        });
        app.model = "qwen".into();
        assert_eq!(
            app.llm(Mode::Act),
            llm("qwen", Effort::Default),
            "not yet saved"
        );
        assert_eq!(app.llm(Mode::Plan), llm("kimi", Effort::High));
    }
}
