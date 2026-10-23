//! The agent picker: what later turns run as. Opened over the prompt by
//! `/agents`, it lists plan and act, which Tab also switches between, then
//! the primary agents, which run in their place until Tab or the picker
//! goes back to a mode.

mod view;

use nth_context::Agents;
use nth_protocol::Mode;
pub use view::draw;

use crate::fuzzy::Filter;

/// One row: a mode, or a primary agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    Mode(Mode),
    Agent(String),
}

#[derive(Debug)]
struct Item {
    choice: Choice,
    about: String,
}

impl Item {
    fn name(&self) -> &str {
        match &self.choice {
            Choice::Mode(mode) => mode.label(),
            Choice::Agent(name) => name,
        }
    }
}

#[derive(Debug)]
pub struct AgentPicker {
    items: Vec<Item>,
    /// What runs now, marked in the list.
    current: Choice,
    filter: Filter,
    /// The items matching the filter, best first, as indices into `items`.
    shown: Vec<usize>,
    /// The highlighted item, an index into `shown`.
    selected: usize,
}

/// What the modes say about themselves in the list.
fn about(mode: Mode) -> &'static str {
    match mode {
        Mode::Plan => "Plans without changing the project; Tab switches to act",
        Mode::Act => "Changes anything it needs to; Tab switches to plan",
    }
}

impl AgentPicker {
    /// The modes, then the primary agents `agents` offers, highlighting
    /// `current`.
    pub fn new(agents: &Agents, current: Choice) -> Self {
        let modes = Mode::ALL.map(|mode| Item {
            choice: Choice::Mode(mode),
            about: about(mode).into(),
        });
        let agents = agents.primaries().map(|agent| Item {
            choice: Choice::Agent(agent.name.clone()),
            about: agent.description.clone().unwrap_or_default(),
        });
        let items: Vec<Item> = modes.into_iter().chain(agents).collect();
        let selected = items
            .iter()
            .position(|item| item.choice == current)
            .unwrap_or(0);
        Self {
            shown: (0..items.len()).collect(),
            items,
            current,
            filter: Filter::default(),
            selected,
        }
    }

    pub fn next(&mut self) {
        if !self.shown.is_empty() {
            self.selected = (self.selected + 1) % self.shown.len();
        }
    }

    pub fn prev(&mut self) {
        if !self.shown.is_empty() {
            self.selected = (self.selected + self.shown.len() - 1) % self.shown.len();
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
        if self.filter.is_empty() {
            return false;
        }
        self.refilter(Filter::clear);
        true
    }

    /// Changes the query and matches the items again by name and what
    /// they do, highlighting the best match.
    fn refilter(&mut self, change: impl FnOnce(&mut Filter)) {
        change(&mut self.filter);
        let haystacks: Vec<String> = self.items.iter().map(haystack).collect();
        self.shown = self.filter.rank(haystacks.iter().map(String::as_str));
        self.selected = 0;
    }

    /// The highlighted choice.
    pub fn chosen(&self) -> Option<Choice> {
        let i = *self.shown.get(self.selected)?;
        Some(self.items[i].choice.clone())
    }
}

/// What the filter matches an item by: its name, then what it does.
fn haystack(item: &Item) -> String {
    format!("{} {}", item.name(), item.about)
}

#[cfg(test)]
mod tests {
    use nth_context::{Context, Paths};

    use super::*;

    /// The built-ins and a primary agent `review` from a file.
    fn agents() -> (tempfile::TempDir, Agents) {
        let dir = tempfile::tempdir().expect("tempdir");
        let folder = dir.path().join(".nth/agents");
        std::fs::create_dir_all(&folder).expect("dir");
        std::fs::write(
            folder.join("review.md"),
            "---\ndescription: Reviews the diff\nmode: primary\n---\nYou review.\n",
        )
        .expect("writes");
        let agents = Context::discover(dir.path(), &Paths::default()).agents;
        (dir, agents)
    }

    #[test]
    fn lists_the_modes_then_the_primary_agents_on_what_runs() {
        let (_dir, agents) = agents();
        let mut picker = AgentPicker::new(&agents, Choice::Mode(Mode::Act));

        let names: Vec<_> = picker.items.iter().map(Item::name).collect();
        assert_eq!(names, ["plan", "act", "review"], "no subagents");
        assert_eq!(picker.chosen(), Some(Choice::Mode(Mode::Act)));

        picker.next();
        assert_eq!(picker.chosen(), Some(Choice::Agent("review".into())));
        picker.next();
        assert_eq!(picker.chosen(), Some(Choice::Mode(Mode::Plan)), "wraps");
    }

    #[test]
    fn filters_by_name_and_what_it_does() {
        let (_dir, agents) = agents();
        let mut picker = AgentPicker::new(&agents, Choice::Mode(Mode::Plan));

        for c in "diff".chars() {
            picker.insert(c);
        }
        assert_eq!(picker.chosen(), Some(Choice::Agent("review".into())));
        assert!(picker.clear_query());
        assert!(!picker.clear_query());
        assert_eq!(picker.shown.len(), 3);
    }
}
