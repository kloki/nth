//! The plan file: where plan mode writes its plan, and the reminders that
//! tell the model it entered plan mode or left it, copied from opencode's
//! `plan-mode.txt` and `build-switch.txt`.

pub mod edits;

use std::path::{Path, PathBuf};

use uuid::Uuid;

const PLAN_MODE: &str = include_str!("plan_mode.md");
const BUILD_SWITCH: &str = include_str!("build_switch.md");
const APPROVED: &str = include_str!("approved.md");

/// Where the session `id` keeps its plan: `<cwd>/.nth/plans/<id>.md`.
pub fn plan_path(cwd: &Path, id: &Uuid) -> PathBuf {
    cwd.join(".nth").join("plans").join(format!("{id}.md"))
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

/// What `/approve` tells the model, as opencode's plan exit does.
pub fn approved(plan: &Path) -> String {
    APPROVED
        .trim_end()
        .replace("{plan}", &plan.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_lives_under_dot_nth() {
        let id = Uuid::nil();
        assert_eq!(
            plan_path("/repo".as_ref(), &id),
            PathBuf::from(format!("/repo/.nth/plans/{id}.md"))
        );
    }

    #[test]
    fn plan_mode_says_where_the_plan_goes_and_who_approves() {
        let plan = Path::new("/repo/.nth/plans/1.md");

        let fresh = plan_mode_reminder(plan, false, Approver::User);
        assert!(fresh.starts_with("<system-reminder>\nPlan mode is active."));
        assert!(fresh.contains(
            "No plan file exists yet. You should create your plan at /repo/.nth/plans/1.md using the write tool."
        ));
        assert!(fresh.contains("/approve"));
        assert!(!fresh.contains('{'), "every placeholder filled");

        let again = plan_mode_reminder(plan, true, Approver::Nobody);
        assert!(again.contains("A plan file already exists at /repo/.nth/plans/1.md."));
        assert!(!again.contains("/approve"));
    }

    #[test]
    fn approval_names_the_plan() {
        assert_eq!(
            approved(Path::new("/p.md")),
            "The plan at /p.md has been approved, you can now edit files. Execute the plan"
        );
    }

    #[test]
    fn build_switch_points_at_an_existing_plan() {
        let plan = Path::new("/p.md");
        assert!(build_switch_reminder(plan, false).ends_with("as needed.\n</system-reminder>"));
        assert!(build_switch_reminder(plan, true).ends_with(
            "A plan file exists at /p.md. You should execute on the plan defined within it"
        ));
    }
}
