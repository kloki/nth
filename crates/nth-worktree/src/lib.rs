//! Git worktrees under `.nth/worktrees` in the main checkout: the worktree
//! tools move a session into one, and a subagent can work in its own.

mod cleanup;
mod git;

use std::path::{Path, PathBuf};

pub use cleanup::{CleanupOutcome, CleanupResult, SkipReason, cleanup};

/// Ignores everything beside it, itself included, so the user's own
/// `.gitignore` files are never touched.
const GITIGNORE: &str = "# nth's worktrees.\n*\n";

#[derive(Debug, Clone, PartialEq)]
pub struct Worktree {
    pub path: PathBuf,
    /// Named as the worktree is.
    pub branch: String,
    /// `None` when it was there already.
    pub created: Option<Created>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Created {
    /// A branch from HEAD.
    NewBranch,
    ExistingBranch,
}

/// The top of the checkout `cwd` is in.
pub async fn toplevel(cwd: &Path) -> Result<PathBuf, String> {
    git::toplevel(cwd).await
}

/// Where the worktrees of the checkout at `root` live, shared with its main checkout.
fn dir(root: &Path) -> PathBuf {
    nth_directories::Directories::discover(root).worktrees()
}

/// The worktree `name` of the checkout at `root`, on the branch `name`:
/// made on first use, from the branch if it exists and from this checkout's HEAD if not.
pub async fn open(root: &Path, name: &str) -> Result<Worktree, String> {
    if !git::valid_branch(root, name).await {
        return Err(format!("{name:?} is not a valid branch name"));
    }
    let dir = dir(root);
    let path = dir.join(name);
    let created = match path.join(".git").exists() {
        true => None,
        false if path.exists() => {
            return Err(format!("{} exists but is not a worktree", path.display()));
        }
        false => {
            ignore(&dir).await?;
            let create = !git::branch_exists(root, name).await;
            git::add(root, &path, name, create).await?;
            Some(match create {
                true => Created::NewBranch,
                false => Created::ExistingBranch,
            })
        }
    };
    Ok(Worktree {
        path,
        branch: name.to_string(),
        created,
    })
}

/// Makes `dir` with its `.gitignore`, unless it is there already.
async fn ignore(dir: &Path) -> Result<(), String> {
    let gitignore = dir.join(".gitignore");
    tokio::fs::create_dir_all(dir)
        .await
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    if !gitignore.exists() {
        tokio::fs::write(&gitignore, GITIGNORE)
            .await
            .map_err(|e| format!("could not write {}: {e}", gitignore.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn run(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(cwd: &Path) {
        run(
            cwd,
            &[
                "-c",
                "user.name=nth",
                "-c",
                "user.email=nth@example.com",
                "commit",
                "-qam",
                "change",
            ],
        );
    }

    /// A repository with one commit, canonical so it compares with what
    /// git prints.
    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical");
        run(&root, &["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "a").expect("write");
        run(&root, &["add", "."]);
        commit(&root);
        (dir, root)
    }

    #[tokio::test]
    async fn opens_an_ignored_worktree_on_a_new_branch() {
        let (_dir, root) = repo();

        let worktree = open(&root, "feature").await.expect("opened");

        assert_eq!(worktree.path, root.join(".nth/worktrees/feature"));
        assert_eq!(worktree.created, Some(Created::NewBranch));
        assert_eq!(
            run(&worktree.path, &["branch", "--show-current"]),
            "feature"
        );
        assert_eq!(run(&root, &["status", "--porcelain"]), "");
    }

    #[tokio::test]
    async fn opens_an_existing_worktree_without_creating_it_again() {
        let (_dir, root) = repo();
        open(&root, "x").await.expect("first");

        let worktree = open(&root, "x").await.expect("second");

        assert_eq!(worktree.created, None);
    }

    #[tokio::test]
    async fn linked_checkout_shares_placement_and_bases_new_branch_on_its_head() {
        let (_dir, root) = repo();
        let linked = root.join("linked");
        run(
            &root,
            &["worktree", "add", "-b", "source", linked.to_str().unwrap()],
        );
        std::fs::write(linked.join("a.txt"), "linked").unwrap();
        commit(&linked);

        let worktree = open(&linked, "feature").await.expect("opened");
        assert_eq!(worktree.path, root.join(".nth/worktrees/feature"));
        let head = run(&worktree.path, &["rev-parse", "HEAD"]);
        assert_eq!(head, run(&linked, &["rev-parse", "HEAD"]));
        assert_ne!(head, run(&root, &["rev-parse", "HEAD"]));
        assert_eq!(open(&root, "feature").await.unwrap().created, None);
    }

    #[tokio::test]
    async fn checks_out_a_branch_that_exists() {
        let (_dir, root) = repo();
        run(&root, &["branch", "old"]);

        let worktree = open(&root, "old").await.expect("opened");

        assert_eq!(worktree.created, Some(Created::ExistingBranch));
    }

    #[tokio::test]
    async fn cleanup_previews_and_removes_only_clean_registered_worktrees() {
        let (_temp, root) = repo();
        let clean = open(&root, "nested/clean").await.unwrap().path;
        let dirty = open(&root, "dirty").await.unwrap().path;
        let locked = open(&root, "locked").await.unwrap().path;
        let current = open(&root, "current").await.unwrap().path;
        std::fs::write(root.join(".git/info/exclude"), "build/\n").unwrap();
        std::fs::create_dir(clean.join("build")).unwrap();
        std::fs::write(clean.join("build/output"), "output").unwrap();
        std::fs::write(dirty.join("untracked"), "change").unwrap();
        run(&root, &["worktree", "lock", locked.to_str().unwrap()]);
        let external = root.join("external space");
        run(
            &root,
            &[
                "worktree",
                "add",
                "-b",
                "external",
                external.to_str().unwrap(),
            ],
        );
        let unregistered = root.join(".nth/worktrees/unregistered");
        std::fs::create_dir(&unregistered).unwrap();
        let nested = current.join("subdir");
        std::fs::create_dir(&nested).unwrap();
        let preview = cleanup(&nested, true).await.unwrap();
        let outcome = |path: &Path| {
            preview
                .iter()
                .find(|r| r.path == path)
                .unwrap()
                .outcome
                .clone()
        };
        assert_eq!(outcome(&clean), CleanupOutcome::WouldRemove);
        assert_eq!(outcome(&dirty), CleanupOutcome::Skipped(SkipReason::Dirty));
        assert_eq!(
            outcome(&locked),
            CleanupOutcome::Skipped(SkipReason::Locked)
        );
        assert_eq!(
            outcome(&current),
            CleanupOutcome::Skipped(SkipReason::Current)
        );
        assert!(clean.exists());
        let results = cleanup(&nested, false).await.unwrap();
        assert!(
            results
                .iter()
                .any(|r| r.path == clean && r.outcome == CleanupOutcome::Removed)
        );
        assert!(!clean.exists());
        assert!(
            dirty.exists()
                && locked.exists()
                && current.exists()
                && external.exists()
                && unregistered.exists()
        );
        assert_eq!(
            run(&root, &["branch", "--list", "nested/clean"]),
            "nested/clean"
        );
    }

    #[tokio::test]
    async fn cleanup_reports_broken_worktrees_and_continues() {
        let (_temp, root) = repo();
        let broken = open(&root, "broken").await.unwrap().path;
        let clean = open(&root, "clean").await.unwrap().path;
        std::fs::remove_dir_all(&broken).unwrap();
        let results = cleanup(&root, false).await.unwrap();
        assert!(
            results
                .iter()
                .any(|r| r.path == broken && matches!(r.outcome, CleanupOutcome::Failed(_)))
        );
        assert!(
            results
                .iter()
                .any(|r| r.path == clean && r.outcome == CleanupOutcome::Removed)
        );
    }

    #[tokio::test]
    async fn cleanup_skips_tracked_changes_and_dirty_submodules() {
        let (_temp, root) = repo();
        let (_source_temp, source) = repo();
        run(
            &root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                source.to_str().unwrap(),
                "sub",
            ],
        );
        commit(&root);
        let staged = open(&root, "staged").await.unwrap().path;
        let unstaged = open(&root, "unstaged").await.unwrap().path;
        let sub = open(&root, "sub-dirty").await.unwrap().path;
        std::fs::write(staged.join("a.txt"), "staged").unwrap();
        run(&staged, &["add", "a.txt"]);
        std::fs::write(unstaged.join("a.txt"), "unstaged").unwrap();
        run(
            &sub,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "update",
                "--init",
            ],
        );
        std::fs::write(sub.join("sub/untracked"), "dirty").unwrap();
        let results = cleanup(&root, true).await.unwrap();
        for path in [&staged, &unstaged, &sub] {
            assert!(results.iter().any(
                |r| &r.path == path && r.outcome == CleanupOutcome::Skipped(SkipReason::Dirty)
            ));
        }
    }

    #[tokio::test]
    async fn cleanup_handles_worktree_paths_with_spaces_and_newlines() {
        let (_temp, root) = repo();
        let managed = root.join(".nth/worktrees");
        std::fs::create_dir_all(&managed).unwrap();
        let path = managed.join("space and\nnewline");
        run(
            &root,
            &["worktree", "add", "-b", "odd", path.to_str().unwrap()],
        );
        let results = cleanup(&root, false).await.unwrap();
        assert_eq!(
            results,
            vec![CleanupResult {
                path: path.clone(),
                outcome: CleanupOutcome::Removed
            }]
        );
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn refuses_a_bad_name_or_no_repository() {
        let (_dir, root) = repo();
        for name in ["", "../out", "-b", "a..b"] {
            let out = open(&root, name).await;
            assert!(out.expect_err(name).contains("not a valid branch"));
        }

        let plain = tempfile::tempdir().expect("tempdir");
        let out = toplevel(plain.path()).await;
        assert!(
            out.expect_err("no repo")
                .contains("not in a git repository")
        );
    }
}
