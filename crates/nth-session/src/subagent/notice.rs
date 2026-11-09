//! What a subagent's job tells the model through the inbox: its answer, or
//! that it has gone quiet. `inbox::notice` renders them as `<task>`
//! elements beside the other notices.

use std::{fmt, time::Duration};

/// Numbered from 1 for as long as the front-end runs, like a monitor; it is
/// the `task_id` the model continues a subagent with.
pub type SubagentId = u32;

/// A subagent's answer to a task, posted by whoever ran it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskNotice {
    pub id: SubagentId,
    pub name: String,
    pub agent: String,
    pub description: String,
    pub outcome: TaskOutcome,
}

/// A subagent whose turn has said nothing for `quiet`, though it still
/// runs: no reply, no tool call. Told again while it stays silent, so the
/// model can decide to stop it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskStall {
    pub id: SubagentId,
    pub name: String,
    pub agent: String,
    pub description: String,
    pub quiet: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutcome {
    /// The subagent's last answer.
    Completed(String),
    /// Why its turn failed.
    Failed(String),
    /// Someone stopped it before it answered.
    Interrupted,
}

impl TaskOutcome {
    /// The `state` attribute, as opencode names it.
    pub fn state(&self) -> &'static str {
        match self {
            TaskOutcome::Completed(_) => "completed",
            TaskOutcome::Failed(_) => "failed",
            TaskOutcome::Interrupted => "interrupted",
        }
    }
}

impl fmt::Display for TaskOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.state())
    }
}
