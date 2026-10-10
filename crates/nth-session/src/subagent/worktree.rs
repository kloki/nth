//! A subagent's own git worktree, for the task tool's `isolation`: open
//! while a turn runs, removed after one that changed nothing, and named in
//! the answer of one that did.

use std::path::{Path, PathBuf};

use nth_worktree::Worktree;

/// The longest name a description makes, before a `-2` that keeps it free.
const MAX_SLUG: usize = 40;

pub(crate) struct Isolation {
    /// The main checkout, which the worktree belongs to.
    root: PathBuf,
    name: String,
    /// The worktree with the commit it started from; `None` once removed.
    open: Option<(Worktree, String)>,
}

impl Isolation {
    /// Opens a worktree named after `description` in the checkout `cwd` is
    /// in, at once, so a second task with the same description finds the
    /// name taken.
    pub(crate) async fn new(cwd: &Path, description: &str) -> Result<Self, String> {
        let root = nth_worktree::toplevel(cwd).await?;
        let name = free_name(&root, &slug(description)).await;
        let mut isolation = Self {
            root,
            name,
            open: None,
        };
        isolation.enter().await?;
        Ok(isolation)
    }

    /// Where the next turn runs, opened again from HEAD if the last turn's
    /// worktree was removed.
    pub(crate) async fn enter(&mut self) -> Result<PathBuf, String> {
        if let Some((worktree, _)) = &self.open {
            return Ok(worktree.path.clone());
        }
        let worktree = nth_worktree::open(&self.root, &self.name).await?;
        let base = nth_worktree::head(&worktree).await?;
        let path = worktree.path.clone();
        self.open = Some((worktree, base));
        Ok(path)
    }

    /// After a turn: a worktree that changed nothing goes, with its branch;
    /// one that did stays, and the returned line tells the parent where.
    pub(crate) async fn leave(&mut self) -> Option<String> {
        let (worktree, base) = self.open.as_ref()?;
        let changes = match nth_worktree::changes(worktree, base).await {
            Ok(changes) if changes.is_none() => {
                return match nth_worktree::remove(&self.root, worktree).await {
                    Ok(()) => {
                        self.open = None;
                        None
                    }
                    Err(e) => Some(format!(
                        "Its worktree {} could not be removed: {e}",
                        worktree.path.display()
                    )),
                };
            }
            Ok(changes) => {
                let mut what = Vec::new();
                if changes.commits > 0 {
                    let s = if changes.commits == 1 { "" } else { "s" };
                    what.push(format!("{} new commit{s}", changes.commits));
                }
                if changes.uncommitted {
                    what.push("uncommitted changes".into());
                }
                what.join(" and ")
            }
            Err(e) => format!("changes git could not read ({e})"),
        };
        Some(format!(
            "Worked in the worktree {} on branch {}, which has {changes}. Review or merge it from there.",
            worktree.path.display(),
            worktree.branch,
        ))
    }
}

/// `description` as a branch name: lowercase letters and digits, words
/// joined by `-`.
fn slug(description: &str) -> String {
    let words: Vec<String> = description
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut slug = String::new();
    for word in words {
        if !slug.is_empty() && slug.len() + 1 + word.len() > MAX_SLUG {
            break;
        }
        if !slug.is_empty() {
            slug.push('-');
        }
        slug.push_str(&word);
    }
    slug.truncate(MAX_SLUG);
    match slug.is_empty() {
        true => "task".into(),
        false => slug,
    }
}

/// `slug`, or `slug-2`, `slug-3` and on when an earlier task, or a branch
/// of the same name, has it.
async fn free_name(root: &Path, slug: &str) -> String {
    let mut name = slug.to_string();
    let mut n = 1;
    while nth_worktree::taken(root, &name).await {
        n += 1;
        name = format!("{slug}-{n}");
    }
    name
}

#[cfg(test)]
pub(crate) mod tests {
    use std::process::Command;

    use super::*;

    pub(crate) fn git(cwd: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=nth", "-c", "user.email=nth@example.com"])
            .args(args)
            .current_dir(cwd)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A repository with one commit, canonical so it compares with what
    /// git prints.
    pub(crate) fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().canonicalize().expect("canonical");
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["commit", "-q", "--allow-empty", "-m", "init"]);
        (dir, root)
    }

    #[test]
    fn a_description_becomes_a_branch_name() {
        assert_eq!(slug("Fix the login bug!"), "fix-the-login-bug");
        assert_eq!(slug("  ünïcode & co  "), "n-code-co");
        assert_eq!(slug("…"), "task");
        let long = slug("one two three four five six seven eight nine ten");
        assert_eq!(long, "one-two-three-four-five-six-seven-eight");
        assert!(slug(&"x".repeat(60)).len() == MAX_SLUG);
    }

    #[tokio::test]
    async fn a_taken_name_gets_a_number() {
        let (_dir, root) = repo();
        git(&root, &["branch", "fix"]);

        let first = Isolation::new(&root, "Fix").await.expect("first");
        let second = Isolation::new(&root, "Fix").await.expect("second");

        assert_eq!(first.name, "fix-2");
        assert_eq!(second.name, "fix-3");
    }

    #[tokio::test]
    async fn a_turn_that_changed_nothing_leaves_no_worktree() {
        let (_dir, root) = repo();
        let mut isolation = Isolation::new(&root, "look").await.expect("opened");
        let path = isolation.enter().await.expect("entered");
        assert_eq!(path, root.join(".nth/worktrees/look"));

        assert_eq!(isolation.leave().await, None);

        assert!(!path.exists());
        assert!(!nth_worktree::taken(&root, "look").await);
        assert_eq!(isolation.enter().await.expect("again"), path, "made again");
    }

    #[tokio::test]
    async fn a_turn_that_changed_something_keeps_it_and_says_where() {
        let (_dir, root) = repo();
        let mut isolation = Isolation::new(&root, "build").await.expect("opened");
        let path = isolation.enter().await.expect("entered");
        std::fs::write(path.join("a.txt"), "a").expect("write");
        git(&path, &["add", "."]);
        git(&path, &["commit", "-qm", "a"]);
        std::fs::write(path.join("b.txt"), "b").expect("write");

        let note = isolation.leave().await.expect("kept");

        assert!(path.exists());
        assert_eq!(
            note,
            format!(
                "Worked in the worktree {} on branch build, which has 1 new commit and uncommitted changes. Review or merge it from there.",
                path.display()
            )
        );
    }

    #[tokio::test]
    async fn outside_a_repository_there_is_no_worktree() {
        let dir = tempfile::tempdir().expect("tempdir");

        let out = Isolation::new(dir.path(), "x").await;

        assert!(
            out.err()
                .expect("refused")
                .contains("not in a git repository")
        );
    }
}
