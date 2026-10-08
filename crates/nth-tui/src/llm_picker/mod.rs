//! The LLM picker: which LLM later turns go to, and how hard it
//! reasons. Opened over the prompt, it waits for the list if it isn't in yet.

pub mod usage;
mod view;

use std::collections::BTreeSet;

use nth_protocol::{Effort, Failed, Listing, ModelInfo};
pub use view::draw;

use crate::fuzzy::Filter;

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
        /// The providers that could not be asked, shown after the models.
        failed: Vec<Failed>,
        /// With models from several providers, ids keep their `provider/`
        /// prefix.
        prefixed: bool,
        filter: Filter,
        /// The models matching the filter, best first, as indices into
        /// `models`.
        shown: Vec<usize>,
        /// The highlighted model, an index into `shown`.
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
    /// With no model at all, the first provider's failure, if any, is why.
    pub fn load(&mut self, listing: Result<Listing, String>) {
        self.state = match listing {
            Ok(listing) if listing.models.is_empty() => {
                State::Failed(match listing.failed.first() {
                    Some(failed) => format!("{}: {}", failed.origin.name, failed.error),
                    None => "the endpoint lists no models".into(),
                })
            }
            Ok(listing) => {
                let origins: BTreeSet<&str> = listing
                    .models
                    .iter()
                    .filter_map(|m| m.origin.as_ref().map(|o| o.id.as_str()))
                    .collect();
                State::Ready {
                    selected: listing
                        .models
                        .iter()
                        .position(|m| m.id == self.current)
                        .unwrap_or(0),
                    prefixed: origins.len() > 1,
                    shown: (0..listing.models.len()).collect(),
                    filter: Filter::default(),
                    models: listing.models,
                    failed: listing.failed,
                }
            }
            Err(error) => State::Failed(error),
        };
    }

    pub fn next(&mut self) {
        if let State::Ready {
            shown, selected, ..
        } = &mut self.state
            && !shown.is_empty()
        {
            *selected = (*selected + 1) % shown.len();
        }
    }

    pub fn prev(&mut self) {
        if let State::Ready {
            shown, selected, ..
        } = &mut self.state
            && !shown.is_empty()
        {
            *selected = (*selected + shown.len() - 1) % shown.len();
        }
    }

    /// Narrows the list by one more character of the query.
    pub fn insert(&mut self, c: char) {
        self.refilter(|filter| filter.push(c));
    }

    pub fn backspace(&mut self) {
        self.refilter(Filter::pop);
    }

    /// Empties the query; false when it already was.
    pub fn clear_query(&mut self) -> bool {
        let State::Ready { filter, .. } = &self.state else {
            return false;
        };
        if filter.is_empty() {
            return false;
        }
        self.refilter(Filter::clear);
        true
    }

    /// Changes the query and matches the models again, highlighting the
    /// best match.
    fn refilter(&mut self, change: impl FnOnce(&mut Filter)) {
        if let State::Ready {
            models,
            prefixed,
            filter,
            shown,
            selected,
            ..
        } = &mut self.state
        {
            change(filter);
            let haystacks: Vec<String> = models.iter().map(|m| haystack(m, *prefixed)).collect();
            *shown = filter.rank(haystacks.iter().map(String::as_str));
            // Back on the model in use once the query is gone, as on opening.
            *selected = if filter.is_empty() {
                shown
                    .iter()
                    .position(|&i| models[i].id == self.current)
                    .unwrap_or(0)
            } else {
                0
            };
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
            State::Ready {
                models,
                shown,
                selected,
                ..
            } => shown.get(*selected).and_then(|&i| models.get(i)),
            _ => None,
        }
    }
}

/// The id a row shows: whole with `prefixed`, else without its provider.
fn shown_id(model: &ModelInfo, prefixed: bool) -> &str {
    if prefixed {
        model.id.as_str()
    } else {
        model.wire_id()
    }
}

/// What the filter matches a model by: its id as shown, then its name.
fn haystack(model: &ModelInfo, prefixed: bool) -> String {
    format!(
        "{} {}",
        shown_id(model, prefixed),
        model.name.as_deref().unwrap_or("")
    )
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
            origin: None,
        }
    }

    fn ready(current: &str) -> LlmPicker {
        let mut picker = LlmPicker::new(current, Effort::Default);
        picker.load(Ok(vec![model("glm", true), model("plain", false)].into()));
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
    fn typing_narrows_to_the_best_match() {
        let mut picker = ready("glm");
        picker.insert('p');
        picker.insert('l');
        assert_eq!(picker.chosen().map(|c| c.0), Some("plain".into()));
        picker.next();
        assert_eq!(picker.chosen().map(|c| c.0), Some("plain".into()), "alone");

        picker.insert('z');
        assert_eq!(picker.chosen(), None, "nothing matches");
        picker.next();
        assert_eq!(picker.chosen(), None);

        picker.backspace();
        picker.backspace();
        picker.backspace();
        assert_eq!(picker.chosen().map(|c| c.0), Some("glm".into()), "all back");
    }

    #[test]
    fn clearing_an_empty_query_says_so() {
        let mut picker = ready("plain");
        assert!(!picker.clear_query());
        picker.insert('g');
        assert_eq!(picker.chosen().map(|c| c.0), Some("glm".into()));
        assert!(picker.clear_query());
        assert_eq!(
            picker.chosen().map(|c| c.0),
            Some("plain".into()),
            "back on the model in use"
        );
    }

    #[test]
    fn nothing_is_chosen_until_the_list_is_in() {
        let mut picker = LlmPicker::new("glm", Effort::High);
        picker.next();
        assert_eq!(picker.chosen(), None);

        picker.load(Ok(Listing::default()));
        assert!(matches!(picker.state, State::Failed(_)));
        picker.load(Err("offline".into()));
        assert!(matches!(picker.state, State::Failed(ref e) if e == "offline"));
    }
}
