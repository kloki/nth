//! Instruction files: `AGENTS.md`, the open standard, and `CLAUDE.md`, read
//! where a project has no `AGENTS.md`. As in opencode, one global file and
//! the project's files from its root down to the working directory.

use std::path::{Path, PathBuf};

use crate::{Paths, project_root};

/// Tried in order in every directory; the first name found anywhere wins,
/// so a project that keeps both files in sync is not read twice.
const NAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

#[derive(Debug, Clone, PartialEq)]
pub struct Instruction {
    pub path: PathBuf,
    pub content: String,
}

pub(crate) fn discover(cwd: &Path, paths: &Paths, warnings: &mut Vec<String>) -> Vec<Instruction> {
    let mut found = Vec::new();
    if let Some(global) = global(paths).into_iter().find(|path| path.is_file()) {
        found.push(global);
    }
    found.extend(project(cwd));
    found.dedup();
    found
        .into_iter()
        .filter_map(|path| read(path, warnings))
        .collect()
}

/// The global files, best first: nth's own, then opencode's, then Claude
/// Code's. Only the first that exists is read.
fn global(paths: &Paths) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(config_home) = &paths.config_home {
        candidates.push(config_home.join("nth/AGENTS.md"));
        candidates.push(config_home.join("opencode/AGENTS.md"));
    }
    if let Some(home) = &paths.home {
        candidates.push(home.join(".claude/CLAUDE.md"));
    }
    candidates
}

/// Every copy of the winning name from the project root down to `cwd`.
/// Outside a repository only `cwd` itself is looked at.
fn project(cwd: &Path) -> Vec<PathBuf> {
    let dirs: Vec<&Path> = match project_root(cwd) {
        Some(root) => cwd
            .ancestors()
            .take_while(|dir| dir.starts_with(&root))
            .collect(),
        None => vec![cwd],
    };
    for name in NAMES {
        let mut files: Vec<PathBuf> = dirs
            .iter()
            .map(|dir| dir.join(name))
            .filter(|path| path.is_file())
            .collect();
        if !files.is_empty() {
            files.reverse();
            return files;
        }
    }
    Vec::new()
}

/// An empty file says nothing, so it is left out rather than shown as a
/// heading with no body.
fn read(path: PathBuf, warnings: &mut Vec<String>) -> Option<Instruction> {
    match std::fs::read_to_string(&path) {
        Ok(content) if content.trim().is_empty() => None,
        Ok(content) => Some(Instruction { path, content }),
        Err(e) => {
            warnings.push(format!("skipped {}: {e}", path.display()));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    struct Tree(tempfile::TempDir);

    impl Tree {
        fn new() -> Self {
            Self(tempfile::tempdir().expect("tempdir"))
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.0.path().join(rel)
        }

        fn file(&self, rel: &str, content: &str) -> &Self {
            let path = self.path(rel);
            fs::create_dir_all(path.parent().expect("has a parent")).expect("dirs");
            fs::write(path, content).expect("writes");
            self
        }

        fn discover(&self, cwd: &str, paths: &Paths) -> Vec<(String, String)> {
            let mut warnings = Vec::new();
            let found = discover(&self.path(cwd), paths, &mut warnings);
            assert_eq!(warnings, Vec::<String>::new());
            found
                .into_iter()
                .map(|i| {
                    let rel = i.path.strip_prefix(self.0.path()).expect("inside");
                    (rel.display().to_string(), i.content)
                })
                .collect()
        }
    }

    fn no_globals() -> Paths {
        Paths::default()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect()
    }

    #[test]
    fn agents_md_wins_over_claude_md() {
        let tree = Tree::new();
        tree.file("repo/.git/HEAD", "")
            .file("repo/AGENTS.md", "agents")
            .file("repo/CLAUDE.md", "claude");

        assert_eq!(
            tree.discover("repo", &no_globals()),
            pairs(&[("repo/AGENTS.md", "agents")])
        );
    }

    #[test]
    fn claude_md_is_the_fallback() {
        let tree = Tree::new();
        tree.file("repo/.git/HEAD", "")
            .file("repo/CLAUDE.md", "claude");

        assert_eq!(
            tree.discover("repo", &no_globals()),
            pairs(&[("repo/CLAUDE.md", "claude")])
        );
    }

    #[test]
    fn ancestors_stack_from_the_root_down() {
        let tree = Tree::new();
        tree.file("repo/.git/HEAD", "")
            .file("repo/AGENTS.md", "root")
            .file("repo/crates/AGENTS.md", "crates")
            .file("repo/crates/a/CLAUDE.md", "not read: AGENTS.md won")
            .file("repo/crates/a/src/x.rs", "");

        assert_eq!(
            tree.discover("repo/crates/a/src", &no_globals()),
            pairs(&[
                ("repo/AGENTS.md", "root"),
                ("repo/crates/AGENTS.md", "crates"),
            ])
        );
    }

    #[test]
    fn the_walk_stops_at_the_project_root() {
        let tree = Tree::new();
        tree.file("AGENTS.md", "above the repo")
            .file("repo/.git/HEAD", "")
            .file("repo/sub/AGENTS.md", "sub");

        assert_eq!(
            tree.discover("repo/sub", &no_globals()),
            pairs(&[("repo/sub/AGENTS.md", "sub")])
        );
    }

    #[test]
    fn outside_a_repository_only_the_cwd_counts() {
        let tree = Tree::new();
        tree.file("AGENTS.md", "parent")
            .file("dir/AGENTS.md", "dir");

        assert_eq!(
            tree.discover("dir", &no_globals()),
            pairs(&[("dir/AGENTS.md", "dir")])
        );
    }

    #[test]
    fn empty_files_are_left_out() {
        let tree = Tree::new();
        tree.file("repo/.git/HEAD", "")
            .file("repo/AGENTS.md", " \n");

        assert_eq!(tree.discover("repo", &no_globals()), pairs(&[]));
    }

    #[test]
    fn the_first_global_file_comes_first() {
        let tree = Tree::new();
        tree.file("home/.config/opencode/AGENTS.md", "opencode")
            .file("home/.claude/CLAUDE.md", "claude")
            .file("repo/.git/HEAD", "")
            .file("repo/AGENTS.md", "project");
        let paths = Paths {
            home: Some(tree.path("home")),
            config_home: Some(tree.path("home/.config")),
        };

        assert_eq!(
            tree.discover("repo", &paths),
            pairs(&[
                ("home/.config/opencode/AGENTS.md", "opencode"),
                ("repo/AGENTS.md", "project"),
            ])
        );

        tree.file("home/.config/nth/AGENTS.md", "nth");
        assert_eq!(
            tree.discover("repo", &paths)[0],
            ("home/.config/nth/AGENTS.md".into(), "nth".into())
        );
    }

    #[test]
    fn claude_code_global_file_is_the_last_resort() {
        let tree = Tree::new();
        tree.file("home/.claude/CLAUDE.md", "claude");
        let paths = Paths {
            home: Some(tree.path("home")),
            config_home: Some(tree.path("home/.config")),
        };

        assert_eq!(
            tree.discover("home", &paths),
            pairs(&[("home/.claude/CLAUDE.md", "claude")])
        );
    }
}
