//! The git commands behind worktrees.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

use tokio::process::Command;

#[derive(Debug)]
pub(crate) struct Entry {
    pub path: PathBuf,
    pub locked: bool,
    pub detached: bool,
}

/// Git's NUL-delimited porcelain preserves even newlines in paths.
pub(crate) async fn worktrees(cwd: &Path) -> Result<Vec<Entry>, String> {
    let bytes = command(cwd, &["worktree", "list", "--porcelain", "-z"]).await?;
    let mut entries = Vec::new();
    let mut entry: Option<Entry> = None;
    for field in bytes.split(|b| *b == 0) {
        if field.is_empty() {
            if let Some(entry) = entry.take() {
                entries.push(entry);
            }
        } else if let Some(path) = field.strip_prefix(b"worktree ") {
            if entry.is_some() || path.is_empty() {
                return Err("invalid git worktree list".into());
            }
            entry = Some(Entry {
                path: path_from_bytes(path)?,
                locked: false,
                detached: false,
            });
        } else if field == b"locked" || field.starts_with(b"locked ") {
            entry.as_mut().ok_or("invalid git worktree list")?.locked = true;
        } else if field == b"detached" {
            entry.as_mut().ok_or("invalid git worktree list")?.detached = true;
        } else if entry.is_none() {
            return Err("invalid git worktree list".into());
        }
    }
    if entry.is_some() || entries.is_empty() || !bytes.ends_with(&[0, 0]) {
        return Err("invalid git worktree list".into());
    }
    Ok(entries)
}

#[cfg(unix)]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes.to_vec())))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: &[u8]) -> Result<PathBuf, String> {
    String::from_utf8(bytes.to_vec())
        .map(PathBuf::from)
        .map_err(|e| e.to_string())
}

pub(crate) async fn clean(path: &Path) -> Result<bool, String> {
    command(
        path,
        &[
            "-c",
            "status.showUntrackedFiles=all",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
    )
    .await
    .map(|out| out.is_empty())
}

pub(crate) async fn remove(cwd: &Path, path: &Path) -> Result<(), String> {
    let out = Command::new("git")
        .args(["worktree", "remove", "--"])
        .arg(path)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

async fn command(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("could not run git: {e}"))?;
    if out.status.success() {
        Ok(out.stdout)
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

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
