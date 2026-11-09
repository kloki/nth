//! `nth worktree cleanup`: report and remove clean managed worktrees.

use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, anyhow};
use clap::Subcommand;
use nth_icons::icons;
use owo_colors::OwoColorize;
use serde_json::{Value, json};

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
    let mut out = io::stdout().lock();
    let json_output = json || !out.is_terminal();
    report(&mut out, &outcomes, dry_run, json_output)
}

fn report(
    out: &mut impl Write,
    outcomes: &[nth_worktree::CleanupResult],
    dry_run: bool,
    json_output: bool,
) -> Result<()> {
    if json_output {
        writeln!(out, "{}", report_json(outcomes, dry_run))?;
    } else {
        report_text(out, outcomes)?;
    }
    if outcomes
        .iter()
        .any(|item| matches!(item.outcome, nth_worktree::CleanupOutcome::Failed(_)))
    {
        return Err(CleanupFailed.into());
    }
    Ok(())
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

fn report_text(out: &mut impl Write, outcomes: &[nth_worktree::CleanupResult]) -> Result<()> {
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

    #[test]
    fn report_contains_outcomes_and_failure_sets_exit_decision() {
        let outcomes = vec![
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
        ];
        let mut out = Vec::new();
        let error = report(&mut out, &outcomes, true, true).unwrap_err();
        assert!(error.is::<CleanupFailed>());
        let value: Value = serde_json::from_slice(&out).unwrap();
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
        let outcomes = [CleanupResult {
            path: "/repo/b".into(),
            outcome: CleanupOutcome::Skipped(SkipReason::Locked),
        }];
        let mut out = Vec::new();
        report(&mut out, &outcomes, false, false).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("skipped /repo/b: locked"));
        let mut out = Vec::new();
        report(&mut out, &[], false, true).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&out).unwrap(),
            json!({"dry_run": false, "worktrees": []})
        );
    }
}
