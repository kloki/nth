//! The LLM picker: which LLM later turns go to, and how hard it
//! reasons. Opened over the prompt, it waits for the list if it isn't in yet.

pub mod usage;
mod view;

use nth_protocol::{Effort, ModelInfo};
pub use view::draw;

#[derive(Debug)]
pub struct LlmPicker {
    /// The model in use, marked in the list.
    current: String,
    /// Kept while a non-reasoning model is highlighted, so moving back to a
    /// reasoning one restores it.
    effort: Effort,
    state: State,
}

#[derive(Debug)]
enum State {
    Loading,
    Failed(String),
    Ready {
        models: Vec<ModelInfo>,
        selected: usize,
    },
}

impl LlmPicker {
    pub fn new(current: &str, effort: Effort) -> Self {
        Self {
            current: current.to_string(),
            effort,
            state: State::Loading,
        }
    }

    /// Fills in the list, highlighting the model in use when it is listed.
    pub fn load(&mut self, models: Result<Vec<ModelInfo>, String>) {
        self.state = match models {
            Ok(models) if models.is_empty() => State::Failed("the endpoint lists no models".into()),
            Ok(models) => State::Ready {
                selected: models
                    .iter()
                    .position(|m| m.id == self.current)
                    .unwrap_or(0),
                models,
            },
            Err(error) => State::Failed(error),
        };
    }

    pub fn next(&mut self) {
        if let State::Ready { models, selected } = &mut self.state {
            *selected = (*selected + 1) % models.len();
        }
    }

    pub fn prev(&mut self) {
        if let State::Ready { models, selected } = &mut self.state {
            *selected = (*selected + models.len() - 1) % models.len();
        }
    }

    pub fn more(&mut self) {
        if self.selected().is_some_and(|m| m.reasoning) {
            self.effort = self.effort.next();
        }
    }

    pub fn less(&mut self) {
        if self.selected().is_some_and(|m| m.reasoning) {
            self.effort = self.effort.prev();
        }
    }

    /// The highlighted model and the effort to run it at; a model that
    /// doesn't reason is sent none.
    pub fn chosen(&self) -> Option<(String, Effort)> {
        let model = self.selected()?;
        let effort = if model.reasoning {
            self.effort
        } else {
            Effort::Default
        };
        Some((model.id.clone(), effort))
    }

    fn selected(&self) -> Option<&ModelInfo> {
        match &self.state {
            State::Ready { models, selected } => models.get(*selected),
            _ => None,
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn model(id: &str, reasoning: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: None,
            context: None,
            output: None,
            reasoning,
        }
    }

    fn ready(current: &str) -> LlmPicker {
        let mut picker = LlmPicker::new(current, Effort::Default);
        picker.load(Ok(vec![model("glm", true), model("plain", false)]));
        picker
    }

    #[test]
    fn opens_on_the_model_in_use() {
        assert_eq!(ready("plain").chosen().map(|c| c.0), Some("plain".into()));
        assert_eq!(ready("gone").chosen().map(|c| c.0), Some("glm".into()));
    }

    #[test]
    fn arrows_wrap_through_the_models() {
        let mut picker = ready("glm");
        picker.next();
        assert_eq!(picker.chosen().map(|c| c.0), Some("plain".into()));
        picker.next();
        assert_eq!(picker.chosen().map(|c| c.0), Some("glm".into()));
        picker.prev();
        assert_eq!(picker.chosen().map(|c| c.0), Some("plain".into()));
    }

    #[test]
    fn effort_only_moves_for_reasoning_models() {
        let mut picker = ready("glm");
        picker.more();
        picker.more();
        assert_eq!(picker.chosen(), Some(("glm".into(), Effort::Medium)));

        picker.next();
        picker.more();
        assert_eq!(picker.chosen(), Some(("plain".into(), Effort::Default)));

        picker.prev();
        picker.less();
        assert_eq!(picker.chosen(), Some(("glm".into(), Effort::Low)));
    }

    #[test]
    fn nothing_is_chosen_until_the_list_is_in() {
        let mut picker = LlmPicker::new("glm", Effort::High);
        picker.next();
        assert_eq!(picker.chosen(), None);

        picker.load(Ok(Vec::new()));
        assert!(matches!(picker.state, State::Failed(_)));
        picker.load(Err("offline".into()));
        assert!(matches!(picker.state, State::Failed(ref e) if e == "offline"));
    }
}
