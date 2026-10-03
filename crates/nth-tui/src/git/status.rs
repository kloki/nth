//! Reads the working tree's status from git, the way starship counts it.

use std::{path::Path, process::Stdio};

use tokio::process::Command;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitStatus {
    /// `None` on a detached head.
    pub branch: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub conflicted: u32,
    pub staged: u32,
    pub modified: u32,
    pub renamed: u32,
    pub deleted: u32,
    pub untracked: u32,
    pub stashed: bool,
}

/// `Ok(None)` outside a git repository; `Err` when git itself fails.
pub async fn load(cwd: &Path) -> Result<Option<GitStatus>, String> {
    let status = git(cwd, &["status", "--porcelain=v2", "--branch"]).await?;
    if !status.status.success() {
        // Outside a repository is the common case, and not an error.
        return Ok(None);
    }
    let mut parsed = parse(&String::from_utf8_lossy(&status.stdout));
    let stash = git(cwd, &["rev-parse", "--verify", "--quiet", "refs/stash"]).await?;
    parsed.stashed = stash.status.success();
    Ok(Some(parsed))
}

async fn git(cwd: &Path, args: &[&str]) -> Result<std::process::Output, String> {
    Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        // Quitting mid-load aborts the task; git must not outlive it.
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("running git failed: {e}"))
}

/// Parses `git status --porcelain=v2 --branch`.
pub fn parse(porcelain: &str) -> GitStatus {
    let mut status = GitStatus::default();
    for line in porcelain.lines() {
        let mut fields = line.split(' ');
        match fields.next() {
            Some("#") => match (fields.next(), fields.next()) {
                (Some("branch.head"), Some(head)) if head != "(detached)" => {
                    status.branch = Some(head.to_string());
                }
                (Some("branch.ab"), Some(ahead)) => {
                    status.ahead = count(ahead.trim_start_matches('+'));
                    status.behind = fields
                        .next()
                        .map_or(0, |b| count(b.trim_start_matches('-')));
                }
                _ => {}
            },
            Some(kind @ ("1" | "2")) => {
                let Some(xy) = fields.next() else { continue };
                let mut xy = xy.chars();
                let (x, y) = (xy.next().unwrap_or('.'), xy.next().unwrap_or('.'));
                if x != '.' {
                    status.staged += 1;
                }
                if kind == "2" {
                    status.renamed += 1;
                }
                if x == 'D' || y == 'D' {
                    status.deleted += 1;
                }
                if matches!(y, 'M' | 'T') {
                    status.modified += 1;
                }
            }
            Some("u") => status.conflicted += 1,
            Some("?") => status.untracked += 1,
            _ => {}
        }
    }
    status
}

fn count(text: &str) -> u32 {
    text.parse().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "# branch.oid 1234\n# branch.head main\n";

    #[test]
    fn a_clean_tree_has_only_a_branch() {
        assert_eq!(
            parse(HEADER),
            GitStatus {
                branch: Some("main".into()),
                ..GitStatus::default()
            }
        );
    }

    #[test]
    fn counts_each_kind_of_change() {
        let porcelain = format!(
            "{HEADER}# branch.upstream origin/main\n# branch.ab +3 -1\n\
             1 M. N... 100644 100644 100644 a b src/staged.rs\n\
             1 .M N... 100644 100644 100644 a b src/modified.rs\n\
             1 MM N... 100644 100644 100644 a b src/both.rs\n\
             1 .D N... 100644 100644 000000 a b src/gone.rs\n\
             2 R. N... 100644 100644 100644 a b R100 src/new.rs\tsrc/old.rs\n\
             u UU N... 100644 100644 100644 100644 a b c src/conflict.rs\n\
             ? notes.txt\n? scratch.rs\n! target\n"
        );
        assert_eq!(
            parse(&porcelain),
            GitStatus {
                branch: Some("main".into()),
                ahead: 3,
                behind: 1,
                conflicted: 1,
                staged: 3,
                modified: 2,
                renamed: 1,
                deleted: 1,
                untracked: 2,
                stashed: false,
            }
        );
    }

    #[test]
    fn a_detached_head_has_no_branch() {
        let status = parse("# branch.oid 1234\n# branch.head (detached)\n");
        assert_eq!(status.branch, None);
    }

    #[tokio::test]
    async fn outside_a_repository_is_none() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert_eq!(load(dir.path()).await, Ok(None));
    }
}
