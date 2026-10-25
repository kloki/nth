//! Names of nth's tools that more than one crate decides something on: agent
//! permissions, plan mode, the session loop and the TUI all have to agree on
//! which tools change files and which touch the work plan.

/// Tools that change files: kept from a subagent while its parent plans,
/// covered by opencode's `edit` permission, and followed by format notes and
/// language server errors in their results.
pub const WRITERS: [&str; 5] = [
    "write",
    "edit",
    "apply_patch",
    "curio_publish",
    "curio_reply",
];

pub const CURIOS: [&str; 3] = ["curio_publish", "curio_read", "curio_reply"];

/// The tool a subagent on a work item ticks its todo with.
pub const WORK_TODO: &str = "work_todo";

/// Whether the tool `name` changes files.
pub fn writes_files(name: &str) -> bool {
    WRITERS.contains(&name)
}

/// Whether the tool `name` reads or changes the work plan.
pub fn touches_work(name: &str) -> bool {
    name.starts_with("work_")
}
