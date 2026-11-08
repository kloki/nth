//! What happens to a work item, as the model hears it through the inbox.
//! `inbox::notice` renders a notice as a `<work_item>` element beside the
//! other notices.

use std::time::Duration;

use nth_work::{Todo, WorkOutcome};

use crate::SubagentId;

/// What happened to a work item, for the orchestrator that assigned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkNotice {
    /// The cycle's folder, such as `01-foundation`.
    pub cycle: String,
    /// The item's slug in it.
    pub item: String,
    /// The subagent working on it.
    pub task: SubagentId,
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
