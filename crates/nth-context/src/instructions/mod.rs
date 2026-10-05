//! Instruction files: `AGENTS.md`, the open standard, and `CLAUDE.md`, read
//! where a project has no `AGENTS.md`. As in opencode, one global file and
//! the project's files from its root down to the working directory go into
//! the system prompt; files deeper in the tree are attached when the model
//! reads something next to them.

use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::{Paths, project_root};

/// Tried in order in every directory; the first name found anywhere wins,
/// so a project that keeps both files in sync is not read twice.
const NAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

#[derive(Debug, Clone, PartialEq)]
pub struct Instruction {
    pub path: PathBuf,
    pub content: String,
}

/// The files for the system prompt, and the name the project's go by when
/// it has any.
pub(crate) fn discover(
    cwd: &Path,
    paths: &Paths,
    warnings: &mut Vec<String>,
) -> (Vec<Instruction>, Option<&'static str>) {
    let mut found = Vec::new();
    if let Some(global) = global(paths).into_iter().find(|path| path.is_file()) {
        found.push(global);
    }
    let (project, name) = project(cwd);
    found.extend(project);
    found.dedup();
    let instructions = found
        .into_iter()
        .filter_map(|path| read(path, warnings))
        .collect();
    (instructions, name)
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

/// Every copy of the winning name from the project root down to `cwd`, and
/// that name. Outside a repository only `cwd` itself is looked at.
fn project(cwd: &Path) -> (Vec<PathBuf>, Option<&'static str>) {
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
            return (files, Some(name));
        }
    }
    (Vec::new(), None)
}

/// The instruction files between `file` and the project root that are not
/// in `loaded` yet, root first, claiming each in `loaded`. Only files called
/// `name` count when the project has one (the name `discover` settled on);
/// without one the first name found in each directory does. The root's own
/// files are already in the system prompt, so the walk stops below it.
pub async fn nested(
    file: PathBuf,
    cwd: PathBuf,
    name: Option<&'static str>,
    loaded: Arc<Mutex<BTreeSet<PathBuf>>>,
) -> Vec<Instruction> {
    crate::blocking(move || nested_blocking(&file, &cwd, name, &loaded)).await
}

fn nested_blocking(
    file: &Path,
    cwd: &Path,
    name: Option<&'static str>,
    loaded: &Mutex<BTreeSet<PathBuf>>,
) -> Vec<Instruction> {
    let cwd = normalize(cwd);
    let file = normalize(&cwd.join(file));
    let root = project_root(&cwd).unwrap_or(cwd);
    let Some(dir) = file.parent() else {
        return Vec::new();
    };
    let names: &[&str] = match &name {
        Some(name) => std::slice::from_ref(name),
        None => &NAMES,
    };
    let mut found: Vec<PathBuf> = dir
        .ancestors()
        .take_while(|dir| *dir != root && dir.starts_with(&root))
        .filter_map(|dir| {
            names
                .iter()
                .map(|name| dir.join(name))
                .find(|p| p.is_file())
        })
        .filter(|path| {
            // Claimed before reading, so a parallel read never takes it too.
            loaded
                .lock()
                .expect("only poisoned if a holder panicked")
                .insert(path.clone())
        })
        .collect();
    found.reverse();
    let mut ignored = Vec::new();
    found
        .into_iter()
        .filter_map(|path| read(path, &mut ignored))
        .collect()
}

/// Resolves `.` and `..` without touching the file system, so paths the
/// model writes compare equal to the ones found by walking directories.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
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
            let (found, _) = discover(&self.path(cwd), paths, &mut warnings);
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
            ..Paths::default()
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

    fn loaded(paths: &[PathBuf]) -> Arc<Mutex<BTreeSet<PathBuf>>> {
        Arc::new(Mutex::new(paths.iter().cloned().collect()))
    }

    async fn nested_in(
        tree: &Tree,
        file: &str,
        loaded: &Arc<Mutex<BTreeSet<PathBuf>>>,
    ) -> Vec<(String, String)> {
        nested_as(tree, file, None, loaded).await
    }

    async fn nested_as(
        tree: &Tree,
        file: &str,
        name: Option<&'static str>,
        loaded: &Arc<Mutex<BTreeSet<PathBuf>>>,
    ) -> Vec<(String, String)> {
        nested(file.into(), tree.path("repo"), name, loaded.clone())
            .await
            .into_iter()
            .map(|i| {
                let rel = i.path.strip_prefix(tree.0.path()).expect("inside");
                (rel.display().to_string(), i.content)
            })
            .collect()
    }

    #[tokio::test]
    async fn reading_deeper_attaches_each_file_once() {
        let tree = Tree::new();
        tree.file("repo/.git/HEAD", "")
            .file("repo/AGENTS.md", "root")
            .file("repo/a/AGENTS.md", "a")
            .file("repo/a/b/CLAUDE.md", "b")
            .file("repo/a/b/x.rs", "")
            .file("repo/a/b/y.rs", "");
        let loaded = loaded(&[tree.path("repo/AGENTS.md")]);

        assert_eq!(
            nested_in(&tree, "a/b/x.rs", &loaded).await,
            pairs(&[("repo/a/AGENTS.md", "a"), ("repo/a/b/CLAUDE.md", "b")])
        );
        assert_eq!(
            nested_in(&tree, "./a/b/../b/y.rs", &loaded).await,
            pairs(&[])
        );
    }

    #[tokio::test]
    async fn nested_files_keep_to_the_name_the_project_uses() {
        let tree = Tree::new();
        tree.file("repo/.git/HEAD", "")
            .file("repo/AGENTS.md", "root")
            .file("repo/crates/a/CLAUDE.md", "not read: AGENTS.md won")
            .file("repo/crates/a/b/AGENTS.md", "b")
            .file("repo/crates/a/b/x.rs", "");
        let loaded = loaded(&[tree.path("repo/AGENTS.md")]);
        let mut warnings = Vec::new();
        let (_, name) = discover(&tree.path("repo/crates/a/b"), &no_globals(), &mut warnings);
        assert_eq!(name, Some("AGENTS.md"));

        assert_eq!(
            nested_as(&tree, "crates/a/b/x.rs", name, &loaded).await,
            pairs(&[("repo/crates/a/b/AGENTS.md", "b")])
        );
    }

    #[tokio::test]
    async fn the_root_and_outside_files_attach_nothing() {
        let tree = Tree::new();
        tree.file("repo/.git/HEAD", "")
            .file("repo/AGENTS.md", "root")
            .file("repo/x.rs", "")
            .file("elsewhere/AGENTS.md", "elsewhere")
            .file("elsewhere/x.rs", "");
        let loaded = loaded(&[]);

        assert_eq!(nested_in(&tree, "x.rs", &loaded).await, pairs(&[]));
        let outside = tree.path("elsewhere/x.rs").display().to_string();
        assert_eq!(nested_in(&tree, &outside, &loaded).await, pairs(&[]));
    }

    #[tokio::test]
    async fn parallel_reads_attach_a_file_once() {
        let tree = Tree::new();
        tree.file("repo/.git/HEAD", "")
            .file("repo/a/AGENTS.md", "a")
            .file("repo/a/x.rs", "")
            .file("repo/a/y.rs", "");
        let loaded = loaded(&[]);

        let (x, y) = tokio::join!(
            nested_in(&tree, "a/x.rs", &loaded),
            nested_in(&tree, "a/y.rs", &loaded)
        );

        assert_eq!(x.len() + y.len(), 1, "{x:?} {y:?}");
    }

    #[test]
    fn claude_code_global_file_is_the_last_resort() {
        let tree = Tree::new();
        tree.file("home/.claude/CLAUDE.md", "claude");
        let paths = Paths {
            home: Some(tree.path("home")),
            config_home: Some(tree.path("home/.config")),
            ..Paths::default()
        };

        assert_eq!(
            tree.discover("home", &paths),
            pairs(&[("home/.claude/CLAUDE.md", "claude")])
        );
    }
}
