//! Plan and act: Tab switches between them, and each keeps its own model
//! and effort, so planning can run on a different model than acting.
//! Subagents have a third, which by default is the parent's.

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
}

impl ModeLlms {
    /// Both modes on `llm`, subagents on theirs.
    pub fn same(llm: Llm) -> Self {
        Self {
            plan: llm.clone(),
            act: llm,
            subagent: None,
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

impl App {
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

    /// Switches to `mode`, and to the model and effort it runs with. The
    /// next turn runs in it; one running now keeps its mode.
    pub(super) fn set_mode(&mut self, mode: Mode) {
        self.save_llm();
        self.mode = mode;
        self.load_llm();
    }

    /// Keeps the model and effort picked for the current mode, as the
    /// picker changes `model` and `effort` directly.
    pub(super) fn save_llm(&mut self) {
        *self.mode_llms.get_mut(self.mode) = Llm {
            model: self.model.clone(),
            effort: self.effort,
        };
    }

    /// What `mode` runs with: the current mode's live pick, as the picker
    /// changes `model` and `effort` before it is saved.
    pub(super) fn llm(&self, mode: Mode) -> Llm {
        if mode == self.mode {
            Llm {
                model: self.model.clone(),
                effort: self.effort,
            }
        } else {
            self.mode_llms.get(mode).clone()
        }
    }

    /// What the subagents of the next turn run on: their own pick, else
    /// the current mode's.
    pub(super) fn subagent_llm(&self) -> Llm {
        self.mode_llms
            .subagent
            .clone()
            .unwrap_or_else(|| self.llm(self.mode))
    }

    /// What `target` runs on now; for all, the current mode's.
    pub(super) fn llm_of(&self, target: Target) -> Llm {
        match target {
            Target::Plan => self.llm(Mode::Plan),
            Target::Act => self.llm(Mode::Act),
            Target::Subagent => self.subagent_llm(),
            Target::All => self.llm(self.mode),
        }
    }

    /// Puts `target` on `llm`, from the next turn.
    pub(super) fn set_llm(&mut self, target: Target, llm: Llm) {
        let mode = match target {
            Target::Plan => Mode::Plan,
            Target::Act => Mode::Act,
            Target::Subagent => {
                self.mode_llms.subagent = Some(llm);
                return;
            }
            Target::All => {
                self.mode_llms = ModeLlms {
                    plan: llm.clone(),
                    act: llm.clone(),
                    subagent: Some(llm),
                };
                self.load_llm();
                return;
            }
        };
        *self.mode_llms.get_mut(mode) = llm;
        if mode == self.mode {
            self.load_llm();
        }
    }

    fn load_llm(&mut self) {
        let llm = self.mode_llms.get(self.mode);
        self.model = llm.model.clone();
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
        });
        assert_eq!(app.mode, Mode::Act, "a bare session acts");
        assert_eq!(rows(&mut app)[9].trim_end(), " ▎ act  · glm");

        app.apply(Action::NextTab);
        assert_eq!(app.mode, Mode::Plan);
        assert_eq!((app.model.as_str(), app.effort), ("kimi", Effort::High));
        let shown = rows(&mut app);
        assert_eq!(shown[9].trim_end(), " ▎ plan · kimi · high");
        assert_eq!(shown[14].trim_end(), " /repo 0m");
        assert_eq!(
            shown[9].find("· kimi"),
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
    fn each_mode_has_its_llm_while_one_is_picked() {
        let mut app = app().with_mode_llms(ModeLlms {
            plan: llm("kimi", Effort::High),
            act: llm("glm", Effort::Default),
            subagent: None,
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
