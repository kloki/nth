//! The git commands behind worktrees.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use tokio::process::Command;

/// The top of the checkout `cwd` is in.
pub(crate) async fn toplevel(cwd: &Path) -> Result<PathBuf, String> {
    let out = git(cwd, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(|_| format!("{} is not in a git repository", cwd.display()))?;
    Ok(PathBuf::from(out.trim_end()))
}

/// Whether `name` is a branch name git takes.
pub(crate) async fn valid_branch(cwd: &Path, name: &str) -> bool {
    // A leading dash would read as an option.
    !name.starts_with('-')
        && git(cwd, &["check-ref-format", "--branch", name])
            .await
            .is_ok()
}

pub(crate) async fn branch_exists(cwd: &Path, name: &str) -> bool {
    let reference = format!("refs/heads/{name}");
    git(cwd, &["rev-parse", "--verify", "--quiet", &reference])
        .await
        .is_ok()
}

/// Checks out branch `name` at `path`, creating the branch from HEAD when
/// `create`.
pub(crate) async fn add(cwd: &Path, path: &Path, name: &str, create: bool) -> Result<(), String> {
    let path = path.to_string_lossy();
    let args: &[&str] = match create {
        true => &["worktree", "add", "-b", name, &path],
        false => &["worktree", "add", &path, name],
    };
    git(cwd, args).await.map(drop)
}

pub(crate) async fn head(cwd: &Path) -> Result<String, String> {
    Ok(git(cwd, &["rev-parse", "HEAD"]).await?.trim().to_string())
}

/// Whether anything in the checkout differs from HEAD, untracked files
/// included.
pub(crate) async fn dirty(cwd: &Path) -> Result<bool, String> {
    Ok(!git(cwd, &["status", "--porcelain"])
        .await?
        .trim()
        .is_empty())
}

/// Commits on HEAD that `base` does not have.
pub(crate) async fn commits_since(cwd: &Path, base: &str) -> Result<usize, String> {
    let range = format!("{base}..HEAD");
    let count = git(cwd, &["rev-list", "--count", &range]).await?;
    count
        .trim()
        .parse()
        .map_err(|e| format!("unexpected rev-list output {count:?}: {e}"))
}

/// Deletes the worktree at `path`, whatever is in it, and then `branch`.
pub(crate) async fn remove(cwd: &Path, path: &Path, branch: &str) -> Result<(), String> {
    let path = path.to_string_lossy();
    git(cwd, &["worktree", "remove", "--force", &path]).await?;
    git(cwd, &["branch", "-D", branch]).await.map(drop)
}

/// git's stdout, or its stderr as the error.
async fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("could not run git: {e}"))?;
    match out.status.success() {
        true => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
        false => Err(String::from_utf8_lossy(&out.stderr).trim().to_string()),
    }
}
