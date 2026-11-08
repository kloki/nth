//! The work plan as nth-work reads it: cycles of items, each item where
//! it stands and what its subagent made of it. Nothing here touches disk.

use std::{
    fmt,
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::{
    cycle_name,
    format::{RESULT, TODO, one_line, todo_counts, todo_lines},
};

/// Where an item stands. Only the orchestrator closes one, as done or
/// failed; a subagent's return leaves it for the orchestrator to judge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Pending,
    Running,
    Returned,
    Done,
    Failed,
    /// A file that does not read as an item, and why: shown as it is, for
    /// the orchestrator to write again.
    Broken(String),
}

impl Status {
    pub fn name(&self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Running => "running",
            Status::Returned => "returned",
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Broken(_) => "broken",
        }
    }

    pub(crate) fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "pending" => Status::Pending,
            "running" => Status::Running,
            "returned" => Status::Returned,
            "done" => Status::Done,
            "failed" => Status::Failed,
            _ => return None,
        })
    }

    /// Not yet closed by the orchestrator, which keeps its cycle open.
    pub fn is_open(&self) -> bool {
        !matches!(self, Status::Done | Status::Failed)
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which item: its cycle's folder and its slug, `01-foundation/store`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ItemRef {
    pub cycle: String,
    pub item: String,
}

impl ItemRef {
    pub fn path(&self, dir: &Path) -> PathBuf {
        dir.join(&self.cycle).join(format!("{}.md", self.item))
    }
}

impl fmt::Display for ItemRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.cycle, self.item)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub at: ItemRef,
    pub title: String,
    pub status: Status,
    /// Who it was last assigned to, and as which subagent of the process
    /// that did: ids start again with every run of nth.
    pub agent: Option<String>,
    /// The subagent's number in the run of nth that assigned it.
    pub task: Option<u32>,
    pub assigned: Option<jiff::Timestamp>,
    /// How its subagent's last turn ended, its text being `result`.
    pub outcome: Option<WorkOutcome>,
    /// The orchestrator's word when it closed the item.
    pub note: Option<String>,
    /// The brief, markdown.
    pub instruction: String,
    pub todos: Vec<Todo>,
    /// The subagent's last answer or error.
    pub result: Option<String>,
    /// When the file last changed: the item's last activity.
    pub modified: Option<SystemTime>,
}

impl Item {
    /// A new item, nobody on it yet.
    pub fn pending(at: ItemRef, title: &str, instruction: &str, todos: Vec<Todo>) -> Self {
        Self {
            at,
            title: one_line(title),
            status: Status::Pending,
            agent: None,
            task: None,
            assigned: None,
            outcome: None,
            note: None,
            instruction: instruction.trim().to_string(),
            todos,
            result: None,
            modified: None,
        }
    }

    /// Ticked steps and all of them.
    pub fn progress(&self) -> (usize, usize) {
        todo_counts(&self.todos)
    }

    /// The item as its subagent reads it: title, brief, todo and the last
    /// result, the frontmatter left out.
    pub fn body(&self) -> String {
        let mut out = format!("# {}\n", self.title);
        if !self.instruction.is_empty() {
            out.push_str(&format!("\n{}\n", self.instruction));
        }
        out.push_str(&format!("\n{TODO}\n\n"));
        if !self.todos.is_empty() {
            out.push_str(&todo_lines(&self.todos));
            out.push('\n');
        }
        if let Some(result) = &self.result {
            out.push_str(&format!("\n{RESULT}\n\n{}\n", result.trim()));
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cycle {
    pub number: u32,
    pub slug: String,
    pub items: Vec<Item>,
}

impl Cycle {
    /// Its folder's name, `01-foundation`.
    pub fn name(&self) -> String {
        cycle_name(self.number, &self.slug)
    }

    /// Whether an item in it is not yet closed.
    pub fn is_open(&self) -> bool {
        self.items.iter().any(|item| item.status.is_open())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// The first heading of `plan.md`, which the orchestrator writes itself.
    pub title: Option<String>,
    /// The rest of `plan.md`.
    pub goal: Option<String>,
    /// By number.
    pub cycles: Vec<Cycle>,
    /// Folders in the work directory that are not a cycle: not named
    /// `NN-slug`, or a second folder for a cycle's number.
    pub ignored: Vec<String>,
}

impl Plan {
    /// The serial guard: the first cycle with an item not yet closed. Its
    /// items are the ones that may be assigned.
    pub fn first_open_cycle(&self) -> Option<&Cycle> {
        self.cycles.iter().find(|cycle| cycle.is_open())
    }

    pub fn cycle(&self, number: u32) -> Option<&Cycle> {
        self.cycles.iter().find(|cycle| cycle.number == number)
    }

    pub fn item(&self, at: &ItemRef) -> Option<&Item> {
        self.cycles
            .iter()
            .filter(|cycle| cycle.name() == at.cycle)
            .flat_map(|cycle| &cycle.items)
            .find(|item| item.at == *at)
    }

    pub fn items(&self) -> impl Iterator<Item = &Item> {
        self.cycles.iter().flat_map(|cycle| &cycle.items)
    }
}

/// One step of a work item's todo list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    pub text: String,
    pub done: bool,
}

/// How the subagent on a work item ended its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkOutcome {
    /// Its last answer.
    Completed(String),
    /// Why its turn failed.
    Failed(String),
    /// Someone stopped it before it answered.
    Interrupted,
    /// It ran past its time; the error says how long that was.
    TimedOut(String),
}

impl WorkOutcome {
    /// The `outcome` attribute, and the item file's `outcome` key.
    pub fn name(&self) -> &'static str {
        match self {
            WorkOutcome::Completed(_) => "completed",
            WorkOutcome::Failed(_) => "failed",
            WorkOutcome::Interrupted => "interrupted",
            WorkOutcome::TimedOut(_) => "timed_out",
        }
    }

    /// The outcome a name stands for, without its text: an item file keeps
    /// the text apart, under its result.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "completed" => WorkOutcome::Completed(String::new()),
            "failed" => WorkOutcome::Failed(String::new()),
            "interrupted" => WorkOutcome::Interrupted,
            "timed_out" => WorkOutcome::TimedOut(String::new()),
            _ => return None,
        })
    }

    /// The answer or the error, empty for a stopped one.
    pub fn text(&self) -> &str {
        match self {
            WorkOutcome::Completed(text)
            | WorkOutcome::Failed(text)
            | WorkOutcome::TimedOut(text) => text,
            WorkOutcome::Interrupted => "",
        }
    }
}

impl fmt::Display for WorkOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}
