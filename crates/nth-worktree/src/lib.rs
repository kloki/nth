//! Git worktrees under `.nth/worktrees` in the main checkout: the worktree
//! tools move a session into one, and a subagent can work in its own.

mod git;

use std::path::{Path, PathBuf};

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

/// What a worktree has that the commit it started from has not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Changes {
    pub uncommitted: bool,
    pub commits: usize,
}

impl Changes {
    pub fn is_none(&self) -> bool {
        !self.uncommitted && self.commits == 0
    }
}

/// The top of the checkout `cwd` is in, where its worktrees go.
pub async fn toplevel(cwd: &Path) -> Result<PathBuf, String> {
    git::toplevel(cwd).await
}

/// Where the worktrees of the checkout at `root` live.
pub fn dir(root: &Path) -> PathBuf {
    root.join(".nth").join("worktrees")
}

/// Whether `name` is taken, as a worktree or as a branch.
pub async fn taken(root: &Path, name: &str) -> bool {
    dir(root).join(name).exists() || git::branch_exists(root, name).await
}

/// The worktree `name` of the checkout at `root`, on the branch `name`:
/// made on first use, from the branch if it exists and from HEAD if not.
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

/// The commit the worktree's HEAD is on.
pub async fn head(worktree: &Worktree) -> Result<String, String> {
    git::head(&worktree.path).await
}

/// What changed in the worktree since `base`, a commit from [`head`].
pub async fn changes(worktree: &Worktree, base: &str) -> Result<Changes, String> {
    Ok(Changes {
        uncommitted: git::dirty(&worktree.path).await?,
        commits: git::commits_since(&worktree.path, base).await?,
    })
}

/// Deletes the worktree and its branch, whatever is in them.
pub async fn remove(root: &Path, worktree: &Worktree) -> Result<(), String> {
    git::remove(root, &worktree.path, &worktree.branch).await
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
        assert!(taken(&root, "x").await);
        assert!(!taken(&root, "y").await);
    }

    #[tokio::test]
    async fn checks_out_a_branch_that_exists() {
        let (_dir, root) = repo();
        run(&root, &["branch", "old"]);

        let worktree = open(&root, "old").await.expect("opened");

        assert_eq!(worktree.created, Some(Created::ExistingBranch));
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

    #[tokio::test]
    async fn sees_uncommitted_changes_and_new_commits() {
        let (_dir, root) = repo();
        let worktree = open(&root, "x").await.expect("opened");
        let base = head(&worktree).await.expect("head");
        assert!(changes(&worktree, &base).await.expect("changes").is_none());

        std::fs::write(worktree.path.join("a.txt"), "b").expect("write");
        let dirty = changes(&worktree, &base).await.expect("changes");
        assert_eq!(
            dirty,
            Changes {
                uncommitted: true,
                commits: 0
            }
        );

        commit(&worktree.path);
        let committed = changes(&worktree, &base).await.expect("changes");
        assert_eq!(
            committed,
            Changes {
                uncommitted: false,
                commits: 1
            }
        );
    }

    #[tokio::test]
    async fn remove_deletes_the_worktree_and_its_branch() {
        let (_dir, root) = repo();
        let worktree = open(&root, "x").await.expect("opened");
        std::fs::write(worktree.path.join("new.txt"), "n").expect("write");

        remove(&root, &worktree).await.expect("removed");

        assert!(!worktree.path.exists());
        assert!(!taken(&root, "x").await);
    }
}
