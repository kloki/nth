//! What happens to a work item, as the model hears it through the inbox
//! and an item file keeps it. `inbox::notice` renders a notice as a
//! `<work_item>` element beside the other notices.

use std::{fmt, time::Duration};

use crate::subagent::TaskId;

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

/// What happened to a work item, for the orchestrator that assigned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkNotice {
    /// The cycle's folder, such as `01-foundation`.
    pub cycle: String,
    /// The item's slug in it.
    pub item: String,
    /// The subagent working on it.
    pub task: TaskId,
    pub agent: String,
    pub state: WorkState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkState {
    /// The subagent changed the todo list; the whole list as it is now.
    Todo(Vec<Todo>),
    /// The subagent's turn ended, with how many steps it had ticked.
    Returned {
        outcome: WorkOutcome,
        done: usize,
        total: usize,
    },
    /// Nothing heard from the subagent for this long; it still runs.
    Stalled { quiet: Duration },
}

impl WorkState {
    /// The `state` attribute.
    pub fn name(&self) -> &'static str {
        match self {
            WorkState::Todo(_) => "todo",
            WorkState::Returned { .. } => "returned",
            WorkState::Stalled { .. } => "stalled",
        }
    }
}

/// The todo list as markdown checkboxes, one step a line, the way an item
/// file keeps it. A step's text stays on its line.
pub(crate) fn todo_lines(todos: &[Todo]) -> String {
    todos
        .iter()
        .map(|todo| {
            let mark = if todo.done { 'x' } else { ' ' };
            format!("- [{mark}] {}", one_line(&todo.text))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Ticked steps and all of them.
pub(crate) fn todo_counts(todos: &[Todo]) -> (usize, usize) {
    (todos.iter().filter(|todo| todo.done).count(), todos.len())
}

/// Text folded onto one line, for a todo step or a note.
pub(crate) fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
