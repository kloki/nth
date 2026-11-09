//! Explicit cleanup of registered worktrees in the repository's nth store.

use std::path::{Path, PathBuf};

use crate::git;

/// What happened to one managed worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupOutcome {
    Removed,
    WouldRemove,
    Skipped(SkipReason),
    Failed(String),
}

/// An ordinary reason to leave a worktree in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    Current,
    Locked,
    Dirty,
    Detached,
    ContainsWorktree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupResult {
    pub path: PathBuf,
    pub outcome: CleanupOutcome,
}

/// Remove clean, unlocked registered worktrees under the main checkout's nth
/// worktree directory. A dry run performs the same checks without removing.
/// Discovery errors are returned; individual worktree errors are results, so
/// they do not prevent other worktrees from being considered.
pub async fn cleanup(cwd: &Path, dry_run: bool) -> Result<Vec<CleanupResult>, String> {
    let entries = git::worktrees(cwd).await?;
    let main = entries
        .first()
        .ok_or("no main worktree")?
        .path
        .canonicalize()
        .map_err(|e| format!("could not resolve main checkout: {e}"))?;
    let current = cwd
        .canonicalize()
        .map_err(|e| format!("could not resolve {}: {e}", cwd.display()))?;
    let managed = nth_directories::Directories::from_root(&main).worktrees();
    // A symlinked store could make a checkout outside the intended layout
    // appear managed, even if its destination is elsewhere in the repo.
    for path in [main.join(".nth"), managed.clone()] {
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(format!("{} is a symlink", path.display()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(format!("could not inspect {}: {e}", path.display())),
            _ => {}
        }
    }
    let scope = match managed.canonicalize() {
        Ok(path) if path.starts_with(&main) && path != main => path,
        Ok(_) => {
            return Err(format!(
                "{} resolves outside the main checkout",
                managed.display()
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("could not resolve {}: {e}", managed.display())),
    };
    let mut results = Vec::new();
    for entry in entries.iter().skip(1) {
        // Require both the registered name and its target to be inside the store.
        // This excludes aliases into the store as well as links out of it.
        if !entry.path.starts_with(&managed) || entry.path == managed {
            continue;
        }
        let path = entry.path.clone();
        let outcome = match eligible(&entries, entry, &scope, &current).await {
            Ok(Some(reason)) => CleanupOutcome::Skipped(reason),
            Ok(None) if dry_run => CleanupOutcome::WouldRemove,
            Ok(None) => {
                // Locks and registration can change while earlier entries are processed.
                match recheck(cwd, &path, &scope, &current).await {
                    Ok(Some(reason)) => CleanupOutcome::Skipped(reason),
                    Ok(None) => match git::remove(cwd, &path).await {
                        Ok(()) => CleanupOutcome::Removed,
                        Err(e) => CleanupOutcome::Failed(e),
                    },
                    Err(e) => CleanupOutcome::Failed(e),
                }
            }
            Err(e) => CleanupOutcome::Failed(e),
        };
        results.push(CleanupResult { path, outcome });
    }
    Ok(results)
}

async fn recheck(
    cwd: &Path,
    path: &Path,
    scope: &Path,
    current: &Path,
) -> Result<Option<SkipReason>, String> {
    let entries = git::worktrees(cwd).await?;
    let entry = entries
        .iter()
        .skip(1)
        .find(|e| e.path == path)
        .ok_or_else(|| "worktree is no longer registered".to_string())?;
    eligible(&entries, entry, scope, current).await
}

async fn eligible(
    entries: &[git::Entry],
    entry: &git::Entry,
    scope: &Path,
    current: &Path,
) -> Result<Option<SkipReason>, String> {
    let path = &entry.path;
    let resolved = path
        .canonicalize()
        .map_err(|e| format!("could not resolve {}: {e}", path.display()))?;
    if !resolved.starts_with(scope) || resolved == scope {
        return Err(format!(
            "{} resolves outside the worktree directory",
            path.display()
        ));
    }
    if current.starts_with(&resolved) {
        return Ok(Some(SkipReason::Current));
    }
    if entry.locked {
        return Ok(Some(SkipReason::Locked));
    }
    if entry.detached {
        return Ok(Some(SkipReason::Detached));
    }
    if entries
        .iter()
        .any(|other| other.path != *path && other.path.starts_with(path))
    {
        return Ok(Some(SkipReason::ContainsWorktree));
    }
    if !git::clean(path).await? {
        return Ok(Some(SkipReason::Dirty));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("file"), "initial").unwrap();
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ],
        );
        (dir, root)
    }

    fn outcome<'a>(results: &'a [CleanupResult], path: &Path) -> &'a CleanupOutcome {
        &results
            .iter()
            .find(|result| result.path == path)
            .unwrap()
            .outcome
    }

    #[tokio::test]
    async fn cleanup_previews_and_removes_only_eligible_worktrees() {
        let (_dir, root) = repo();
        let clean = crate::open(&root, "nested/clean").await.unwrap().path;
        let dirty = crate::open(&root, "dirty").await.unwrap().path;
        let locked = crate::open(&root, "locked").await.unwrap().path;
        let detached = crate::open(&root, "detached").await.unwrap().path;
        let current = crate::open(&root, "current").await.unwrap().path;
        std::fs::create_dir(clean.join("target")).unwrap();
        std::fs::write(clean.join("target/cache"), "cache").unwrap();
        std::fs::write(dirty.join("untracked"), "data").unwrap();
        git(&root, &["worktree", "lock", locked.to_str().unwrap()]);
        git(&detached, &["checkout", "--detach"]);
        let unmanaged = root.join("external");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "external",
                unmanaged.to_str().unwrap(),
            ],
        );
        let stray = root.join(".nth/worktrees/stray");
        std::fs::create_dir(&stray).unwrap();

        let preview = cleanup(&current, true).await.unwrap();
        assert_eq!(outcome(&preview, &clean), &CleanupOutcome::WouldRemove);
        assert_eq!(
            outcome(&preview, &dirty),
            &CleanupOutcome::Skipped(SkipReason::Dirty)
        );
        assert_eq!(
            outcome(&preview, &locked),
            &CleanupOutcome::Skipped(SkipReason::Locked)
        );
        assert_eq!(
            outcome(&preview, &detached),
            &CleanupOutcome::Skipped(SkipReason::Detached)
        );
        assert_eq!(
            outcome(&preview, &current),
            &CleanupOutcome::Skipped(SkipReason::Current)
        );
        assert!(clean.exists());
        let result = cleanup(&current, false).await.unwrap();
        assert_eq!(outcome(&result, &clean), &CleanupOutcome::Removed);
        assert!(!clean.exists());
        assert!(dirty.exists() && locked.exists() && detached.exists() && current.exists());
        assert!(unmanaged.exists() && stray.exists());
        assert_eq!(
            git(&root, &["branch", "--list", "nested/clean"]),
            "nested/clean"
        );
    }

    #[tokio::test]
    async fn staged_and_modified_files_are_dirty() {
        let (_dir, root) = repo();
        let staged = crate::open(&root, "staged").await.unwrap().path;
        let changed = crate::open(&root, "changed").await.unwrap().path;
        std::fs::write(staged.join("file"), "staged").unwrap();
        git(&staged, &["add", "file"]);
        std::fs::write(changed.join("file"), "changed").unwrap();
        let results = cleanup(&root, false).await.unwrap();
        for path in [&staged, &changed] {
            assert_eq!(
                outcome(&results, path),
                &CleanupOutcome::Skipped(SkipReason::Dirty)
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn refuses_symlinked_store() {
        use std::os::unix::fs::symlink;
        let (_dir, root) = repo();
        std::fs::create_dir(root.join(".nth")).unwrap();
        symlink(&root, root.join(".nth/worktrees")).unwrap();
        assert!(cleanup(&root, false).await.unwrap_err().contains("symlink"));
    }
}
