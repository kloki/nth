//! `nth worktree cleanup`: report and remove clean managed worktrees.

use std::io::Write;

use anyhow::{Context, Result, anyhow};
use clap::Subcommand;
use nth_icons::icons;
use owo_colors::OwoColorize;
use serde_json::{Value, json};

use crate::output::{self, Report};

#[derive(Subcommand)]
pub enum WorktreeCommand {
    /// Remove clean managed worktrees, keeping their branches
    ///
    /// Ignored files, including local data and build output, are deleted.
    /// Dirty, locked, detached and current worktrees are skipped.
    Cleanup {
        /// Report what would be removed without removing it
        #[arg(long)]
        dry_run: bool,
        /// Print JSON, the default when stdout is not a terminal
        #[arg(long)]
        json: bool,
    },
}

/// The individual failures were already printed in the report.
#[derive(Debug)]
pub struct CleanupFailed;

impl std::fmt::Display for CleanupFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("worktree cleanup failed")
    }
}
impl std::error::Error for CleanupFailed {}

pub async fn run(command: WorktreeCommand) -> Result<()> {
    let WorktreeCommand::Cleanup { dry_run, json } = command;
    let cwd = std::env::current_dir().context("no working directory")?;
    let outcomes = nth_worktree::cleanup(&cwd, dry_run)
        .await
        .map_err(|e| anyhow!(e))?;
    let report = WorktreeReport { outcomes, dry_run };
    output::print(&report, json)?;
    report.exit()
}

/// What `nth worktree cleanup` did, or would do, with each managed worktree.
struct WorktreeReport {
    outcomes: Vec<nth_worktree::CleanupResult>,
    dry_run: bool,
}

impl WorktreeReport {
    /// Fails once the report is printed when any worktree could not be
    /// removed, so the command exits non-zero; the report named why.
    fn exit(&self) -> Result<()> {
        if self
            .outcomes
            .iter()
            .any(|item| matches!(item.outcome, nth_worktree::CleanupOutcome::Failed(_)))
        {
            return Err(CleanupFailed.into());
        }
        Ok(())
    }
}

impl Report for WorktreeReport {
    fn text(&self, out: &mut dyn Write) -> Result<()> {
        report_text(out, &self.outcomes)
    }

    fn json(&self, out: &mut dyn Write) -> Result<()> {
        writeln!(out, "{}", report_json(&self.outcomes, self.dry_run))?;
        Ok(())
    }
}

fn report_json(outcomes: &[nth_worktree::CleanupResult], dry_run: bool) -> Value {
    let entries: Vec<Value> = outcomes
        .iter()
        .map(|item| {
            let (status, reason) = match &item.outcome {
                nth_worktree::CleanupOutcome::Removed => ("removed", None),
                nth_worktree::CleanupOutcome::WouldRemove => ("would_remove", None),
                nth_worktree::CleanupOutcome::Skipped(reason) => {
                    ("skipped", Some(skip_reason(*reason)))
                }
                nth_worktree::CleanupOutcome::Failed(reason) => ("failed", Some(reason.as_str())),
            };
            json!({"path": item.path.to_string_lossy(), "status": status, "reason": reason})
        })
        .collect();
    json!({"dry_run": dry_run, "worktrees": entries})
}

fn skip_reason(reason: nth_worktree::SkipReason) -> &'static str {
    match reason {
        nth_worktree::SkipReason::Current => "current worktree",
        nth_worktree::SkipReason::Locked => "locked",
        nth_worktree::SkipReason::Dirty => "dirty",
        nth_worktree::SkipReason::Detached => "detached HEAD",
        nth_worktree::SkipReason::ContainsWorktree => "contains another worktree",
    }
}

fn report_text(out: &mut dyn Write, outcomes: &[nth_worktree::CleanupResult]) -> Result<()> {
    if outcomes.is_empty() {
        writeln!(
            out,
            "{} {}",
            icons().to.cyan().bold(),
            "no managed worktrees".dimmed()
        )?;
    }
    for item in outcomes {
        match &item.outcome {
            nth_worktree::CleanupOutcome::Removed => writeln!(
                out,
                "{} removed {}",
                icons().ok.green().bold(),
                item.path.display()
            )?,
            nth_worktree::CleanupOutcome::WouldRemove => writeln!(
                out,
                "{} would remove {}",
                icons().to.cyan().bold(),
                item.path.display()
            )?,
            nth_worktree::CleanupOutcome::Skipped(reason) => writeln!(
                out,
                "{} skipped {}: {}",
                icons().warn.yellow().bold(),
                item.path.display(),
                skip_reason(*reason)
            )?,
            nth_worktree::CleanupOutcome::Failed(reason) => writeln!(
                out,
                "{} failed {}: {reason}",
                icons().fail.red().bold(),
                item.path.display()
            )?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use nth_worktree::{CleanupOutcome, CleanupResult, SkipReason};

    use super::*;
    use crate::output::testing;

    /// One worktree with each outcome.
    fn outcomes() -> Vec<CleanupResult> {
        let at = |path: &str, outcome| CleanupResult {
            path: path.into(),
            outcome,
        };
        vec![
            at("/repo/.nth/worktrees/a", CleanupOutcome::Removed),
            at("/repo/.nth/worktrees/b", CleanupOutcome::WouldRemove),
            at(
                "/repo/.nth/worktrees/c",
                CleanupOutcome::Skipped(SkipReason::Dirty),
            ),
            at(
                "/repo/.nth/worktrees/d",
                CleanupOutcome::Skipped(SkipReason::ContainsWorktree),
            ),
            at(
                "/repo/.nth/worktrees/e",
                CleanupOutcome::Failed("permission denied".into()),
            ),
        ]
    }

    #[test]
    fn each_outcome_is_a_line() {
        // Mixes the dry run's `would remove` into a real run on purpose,
        // to show every outcome's line in one snapshot.
        let report = WorktreeReport {
            outcomes: outcomes(),
            dry_run: false,
        };

        let text = testing::text(&report);

        insta::assert_snapshot!(text);
    }

    #[test]
    fn no_managed_worktrees_says_so() {
        let report = WorktreeReport {
            outcomes: Vec::new(),
            dry_run: false,
        };

        let text = testing::text(&report);

        insta::assert_snapshot!(text);
    }

    #[test]
    fn the_outcomes_are_one_json_document() {
        // A real run: a dry run never removes or fails.
        let report = WorktreeReport {
            outcomes: outcomes()
                .into_iter()
                .filter(|item| !matches!(item.outcome, CleanupOutcome::WouldRemove))
                .collect(),
            dry_run: false,
        };

        let value: Value = serde_json::from_str(&testing::json(&report)).unwrap();

        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn report_contains_outcomes_and_failure_sets_exit_decision() {
        let report = WorktreeReport {
            outcomes: vec![
                CleanupResult {
                    path: "/repo/a".into(),
                    outcome: CleanupOutcome::WouldRemove,
                },
                CleanupResult {
                    path: "/repo/b".into(),
                    outcome: CleanupOutcome::Skipped(SkipReason::Dirty),
                },
                CleanupResult {
                    path: "/repo/c".into(),
                    outcome: CleanupOutcome::Failed("refused".into()),
                },
            ],
            dry_run: true,
        };
        let error = report.exit().unwrap_err();
        assert!(error.is::<CleanupFailed>());
        let value: Value = serde_json::from_str(&testing::json(&report)).unwrap();
        assert_eq!(value["dry_run"], true);
        assert_eq!(
            value["worktrees"][0],
            json!({"path": "/repo/a", "status": "would_remove", "reason": null})
        );
        assert_eq!(value["worktrees"][1]["reason"], "dirty");
        assert_eq!(value["worktrees"][2]["status"], "failed");
        assert_eq!(value["worktrees"][2]["reason"], "refused");
    }

    #[test]
    fn skips_and_empty_reports_succeed() {
        let report = WorktreeReport {
            outcomes: vec![CleanupResult {
                path: "/repo/b".into(),
                outcome: CleanupOutcome::Skipped(SkipReason::Locked),
            }],
            dry_run: false,
        };
        report.exit().unwrap();
        let text = testing::text(&report);
        assert!(text.contains("skipped /repo/b: locked"));
        let report = WorktreeReport {
            outcomes: Vec::new(),
            dry_run: false,
        };
        report.exit().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&testing::json(&report)).unwrap(),
            json!({"dry_run": false, "worktrees": []})
        );
    }
}
