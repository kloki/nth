//! Plan and act: Tab switches between them, and each keeps its own model
//! and effort, so planning can run on a different model than acting.

use nth_protocol::{Effort, Mode};

use super::App;

/// A model and the effort it runs at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Llm {
    pub model: String,
    pub effort: Effort,
}

/// What each mode runs with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeLlms {
    pub plan: Llm,
    pub act: Llm,
}

impl ModeLlms {
    /// Both modes on `llm`.
    pub fn same(llm: Llm) -> Self {
        Self {
            plan: llm.clone(),
            act: llm,
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
        self.mode_llms = llms;
        self.load_llm();
        self
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

    fn load_llm(&mut self) {
        let llm = self.mode_llms.get(self.mode);
        self.model = llm.model.clone();
        self.effort = llm.effort;
    }
}

#[cfg(test)]
mod tests {
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
        });
        assert_eq!(app.mode, Mode::Act, "a bare session acts");
        assert_eq!(rows(&mut app)[9].trim_end(), " ▎ act");

        app.apply(Action::NextTab);
        assert_eq!(app.mode, Mode::Plan);
        assert_eq!((app.model.as_str(), app.effort), ("kimi", Effort::High));
        let shown = rows(&mut app);
        assert_eq!(shown[9].trim_end(), " ▎ plan");
        assert_eq!(shown[14].trim_end(), " kimi · high · /repo");

        app.effort = Effort::Low;
        app.apply(Action::PrevTab);
        assert_eq!((app.mode, app.model.as_str()), (Mode::Act, "glm"));
        app.apply(Action::NextTab);
        assert_eq!(app.effort, Effort::Low, "the pick stays with its mode");
    }
}
