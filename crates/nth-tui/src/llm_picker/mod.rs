//! The LLM picker: which LLM later turns go to, and how hard it
//! reasons. Opened over the prompt, it waits for the list if it isn't in yet.
//! Tab picks what the choice is for: plan, act, the subagents or the clerk.

pub mod usage;
mod view;

use std::collections::BTreeSet;

use nth_protocol::{Effort, Failed, Listing, Mode, ModelInfo};
pub use view::draw;

use crate::fuzzy::Filter;

/// What a picked LLM goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Plan,
    Act,
    Subagent,
    /// The built-in clerk.
    Clerk,
}

impl Target {
    pub const ALL: [Target; 4] = [Target::Plan, Target::Act, Target::Subagent, Target::Clerk];

    /// The one for what `mode` runs with.
    pub fn of(mode: Mode) -> Self {
        match mode {
            Mode::Plan => Target::Plan,
            Mode::Act => Target::Act,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Target::Plan => "plan",
            Target::Act => "act",
            Target::Subagent => "subagent",
            Target::Clerk => "clerk",
        }
    }

    /// The next one, wrapping, as Tab moves.
    pub fn next(self) -> Self {
        let i = Self::ALL.iter().position(|&t| t == self).unwrap_or(0);
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    /// The previous one, wrapping, as shift+Tab moves.
    pub fn prev(self) -> Self {
        let i = Self::ALL.iter().position(|&t| t == self).unwrap_or(0);
        Self::ALL[(i + Self::ALL.len() - 1) % Self::ALL.len()]
    }
}

#[derive(Debug)]
pub struct LlmPicker {
    /// What the pick goes to.
    target: Target,
    /// What the target runs on now, marked in the list.
    current: String,
    /// The effort last picked, kept while a model that takes fewer levels
    /// is highlighted, so moving back restores it. Each model is shown and
    /// sent its nearest level.
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
    pub fn new(target: Target, current: &str, effort: Effort) -> Self {
        Self {
            target,
            current: current.to_string(),
            effort,
            state: State::Loading,
        }
    }

    pub fn target(&self) -> Target {
        self.target
    }

    /// Points the pick at `target`, which runs on `current` at `effort`:
    /// that is marked, and highlighted unless a query is narrowing the list.
    pub fn retarget(&mut self, target: Target, current: &str, effort: Effort) {
        self.target = target;
        self.current = current.to_string();
        self.effort = effort;
        self.refilter(|_| {});
    }

    /// Marks `current` at `effort` as what the target runs on now, just
    /// picked: the highlight and the query stay where they are.
    pub fn mark(&mut self, current: &str, effort: Effort) {
        self.current = current.to_string();
        self.effort = effort;
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
        self.step(|i, levels| (i + 1).min(levels - 1));
    }

    pub fn less(&mut self) {
        self.step(|i, _| i.saturating_sub(1));
    }

    /// Moves the effort over `Default` and the highlighted model's levels,
    /// from the one it is shown at. A model without levels stays put.
    fn step(&mut self, to: impl FnOnce(usize, usize) -> usize) {
        let Some(model) = self.selected().filter(|m| !m.efforts.is_empty()) else {
            return;
        };
        let levels: Vec<Effort> = std::iter::once(Effort::Default)
            .chain(model.efforts.iter().copied())
            .collect();
        let shown = self.effort.nearest(&model.efforts);
        let at = levels.iter().position(|&e| e == shown).unwrap_or(0);
        self.effort = levels[to(at, levels.len())];
    }

    /// The effort the highlighted model is shown at, if it takes one.
    fn shown_effort(&self) -> Option<Effort> {
        let model = self.selected().filter(|m| !m.efforts.is_empty())?;
        Some(self.effort.nearest(&model.efforts))
    }

    /// The highlighted model and the effort to run it at: the nearest level
    /// it takes, so a model that takes none is sent none.
    pub fn chosen(&self) -> Option<(String, Effort)> {
        let model = self.selected()?;
        Some((model.id.clone(), self.effort.nearest(&model.efforts)))
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
            efforts: if reasoning {
                vec![Effort::Low, Effort::Medium, Effort::High]
            } else {
                Vec::new()
            },
            origin: None,
            cost: None,
            takes_images: None,
        }
    }

    fn ready(current: &str) -> LlmPicker {
        let mut picker = LlmPicker::new(Target::Act, current, Effort::Default);
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
    fn effort_steps_over_the_levels_the_model_takes() {
        let mut picker = LlmPicker::new(Target::Act, "glm", Effort::Low);
        let mut few = model("few", true);
        few.efforts = vec![Effort::High, Effort::Max];
        picker.load(Ok(vec![model("glm", true), few].into()));
        picker.next();
        assert_eq!(
            picker.chosen(),
            Some(("few".into(), Effort::High)),
            "nearest"
        );
        picker.more();
        picker.more();
        assert_eq!(picker.chosen(), Some(("few".into(), Effort::Max)), "stops");
        picker.less();
        picker.less();
        picker.less();
        assert_eq!(picker.chosen(), Some(("few".into(), Effort::Default)));

        picker.prev();
        picker.more();
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
    fn tab_cycles_the_targets_and_wraps() {
        assert_eq!(Target::of(Mode::Plan).next(), Target::Act);
        assert_eq!(Target::Act.next(), Target::Subagent);
        assert_eq!(Target::Subagent.next(), Target::Clerk);
        assert_eq!(Target::Clerk.next(), Target::Plan);
        assert_eq!(Target::Plan.prev(), Target::Clerk);
    }

    #[test]
    fn retargeting_marks_what_the_target_runs_on() {
        let mut picker = ready("glm");
        picker.retarget(Target::Subagent, "plain", Effort::Default);
        assert_eq!(picker.target(), Target::Subagent);
        assert_eq!(picker.chosen().map(|c| c.0), Some("plain".into()));

        picker.insert('g');
        picker.retarget(Target::Clerk, "plain", Effort::Default);
        assert_eq!(
            picker.chosen().map(|c| c.0),
            Some("glm".into()),
            "the query keeps its best match"
        );
    }

    #[test]
    fn nothing_is_chosen_until_the_list_is_in() {
        let mut picker = LlmPicker::new(Target::Act, "glm", Effort::High);
        picker.next();
        assert_eq!(picker.chosen(), None);

        picker.load(Ok(Listing::default()));
        assert!(matches!(picker.state, State::Failed(_)));
        picker.load(Err("offline".into()));
        assert!(matches!(picker.state, State::Failed(ref e) if e == "offline"));
    }
}
