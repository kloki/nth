//! The plan file: where plan mode writes its plan, and the reminders that
//! tell the model it entered plan mode or left it, based on opencode's
//! `plan-mode.txt` and `build-switch.txt` with nth's own worktree and
//! work-plan guidance added.

pub mod edits;

use std::{
    io,
    path::{Path, PathBuf},
};

use nth_protocol::{Mode, Writable};
use uuid::Uuid;

const PLAN_MODE: &str = include_str!("plan_mode.md");
const BUILD_SWITCH: &str = include_str!("build_switch.md");
const APPROVED: &str = include_str!("approved.md");

/// The project `.nth`'s `.gitignore`: nth's plan and work files stay out
/// of git, while project skills (`.nth/skills`) stay tracked. It lists
/// itself too, so nth's own directory stays invisible to git.
const GITIGNORE: &str = "# nth's plan and work files.\nplans/\nwork/\n.gitignore\n";

/// Where the session `id` keeps its plan: `<cwd>/.nth/plans/<id>.md`.
pub fn plan_path(cwd: &Path, id: &Uuid) -> PathBuf {
    cwd.join(".nth").join("plans").join(format!("{id}.md"))
}

/// Creates `<cwd>/.nth` and, unless one is already there, the
/// `.gitignore` that keeps nth's plan and work files out of git. Called
/// when plan mode starts, so the directory is ready before the model
/// writes the plan, and before a work item is first written.
///
/// A `.gitignore` already there is left alone: the user's rules win.
pub async fn ensure_dir(cwd: &Path) -> io::Result<()> {
    let dir = cwd.join(".nth");
    tokio::fs::create_dir_all(&dir).await?;
    let gitignore = dir.join(".gitignore");
    if !tokio::fs::try_exists(&gitignore).await.unwrap_or(false) {
        tokio::fs::write(&gitignore, GITIGNORE).await?;
    }
    Ok(())
}

/// Who can approve the plan, which decides how the model hands it over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Approver {
    /// A user at the TUI, who approves with `/approve`.
    User,
    /// Nobody: a headless run ends once the plan is written.
    Nobody,
}

/// Appended to the first prompt in plan mode.
pub fn plan_mode_reminder(plan: &Path, exists: bool, approver: Approver) -> String {
    let info = if exists {
        format!(
            "A plan file already exists at {}. You can read it and make incremental edits using the edit tool.",
            plan.display()
        )
    } else {
        format!(
            "No plan file exists yet. You should create your plan at {} using the write tool.",
            plan.display()
        )
    };
    let approve = match approver {
        Approver::User => {
            "The user approves it with /approve, which switches you to act mode to carry it out."
        }
        Approver::Nobody => "Nobody can approve it in this run, so stop once the plan is written.",
    };
    PLAN_MODE
        .replace("{approve}", approve)
        // Last, so a `{approve}` in the path is left alone.
        .replace("{plan_info}", &info)
}

/// Appended to the first prompt in act mode after a turn in plan mode.
pub fn build_switch_reminder(plan: &Path, exists: bool) -> String {
    let reminder = BUILD_SWITCH.trim_end();
    match exists {
        true => format!(
            "{reminder}\n\nA plan file exists at {}. You should execute on the plan defined within it",
            plan.display()
        ),
        false => reminder.to_string(),
    }
}

/// What the model needs to hear about the mode before a turn in `mode`
/// after one in `last`: that plan mode starts, or that it ended. Nothing
/// while the mode stays.
pub(crate) async fn reminder(
    mode: Mode,
    last: Option<Mode>,
    plan: &Path,
    approver: Approver,
) -> Option<String> {
    let entering = mode == Mode::Plan && last != Some(Mode::Plan);
    let leaving = mode == Mode::Act && last == Some(Mode::Plan);
    if !entering && !leaving {
        return None;
    }
    // A check that fails reads as no plan yet, which only changes the
    // wording of the reminder.
    let exists = tokio::fs::try_exists(plan).await.unwrap_or(false);
    Some(match mode {
        Mode::Plan => plan_mode_reminder(plan, exists, approver),
        Mode::Act => build_switch_reminder(plan, exists),
    })
}

/// What tools may write in `mode`: in plan mode only the plan file.
pub(crate) fn writable(mode: Mode, plan: PathBuf) -> Writable {
    match mode {
        Mode::Plan => Writable::Only(plan),
        Mode::Act => Writable::Any,
    }
}

/// What `/approve` tells the model, as opencode's plan exit does.
pub fn approved(plan: &Path) -> String {
    APPROVED
        .trim_end()
        .replace("{plan}", &plan.display().to_string())
}

#[cfg(test)]
mod tests;
