//! Instruction files: `AGENTS.md`, the open standard, and `CLAUDE.md`, read
//! where a project has no `AGENTS.md`. As in opencode, one global file and
//! the files from the working directory up to the home directory (or the
//! project root, outside home) go into the system prompt; files deeper in
//! the tree are attached when the model reads something next to them.
//! Each brings the files it imports with `@path` (`imports`).

mod imports;

use std::{
    collections::BTreeSet,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::{Paths, project_root};

/// Tried in order in every directory.
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
    let global = global(paths).into_iter().find(|path| path.is_file());
    let (project, name) = project(cwd, paths.home.as_deref());
    // The walk reaches the global file too when `cwd` is in its folder.
    let project = project
        .into_iter()
        .filter(|path| Some(path) != global.as_ref());
    let found: Vec<PathBuf> = global.iter().cloned().chain(project).collect();
    // A file the walk found comes in its own place, never as an import.
    let mut seen: BTreeSet<PathBuf> = found.iter().cloned().collect();
    let mut instructions = Vec::new();
    for path in found {
        let Some(instruction) = read(path, warnings) else {
            continue;
        };
        let imported = imports::expand(
            &instruction,
            paths.home.as_deref(),
            &mut imports::once(&mut seen),
            warnings,
        );
        instructions.push(instruction);
        instructions.extend(imported);
    }
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

/// The files from the walk's top down to `cwd`, and the name the project's
/// go by. The walk stops at `home` when `cwd` is under it, even inside a
/// repository, else at the project root, else looks at `cwd` alone. In the
/// project (`cwd` alone outside a repository) the first name found anywhere
/// wins, so a project that keeps both files in sync is not read twice;
/// above it each directory gives its own first, so a `CLAUDE.md` project
/// under a home with an `AGENTS.md` keeps its files.
fn project(cwd: &Path, home: Option<&Path>) -> (Vec<PathBuf>, Option<&'static str>) {
    let root = project_root(cwd);
    let stop = match home {
        Some(home) if cwd.starts_with(home) => Some(home),
        _ => root.as_deref(),
    };
    let dirs: Vec<&Path> = match stop {
        Some(stop) => cwd
            .ancestors()
            .take_while(|dir| dir.starts_with(stop))
            .collect(),
        None => vec![cwd],
    };
    let inside = match &root {
        Some(root) => dirs.iter().take_while(|dir| dir.starts_with(root)).count(),
        None => 1,
    };
    let (inside, above) = dirs.split_at(inside);
    // A worktree under its main checkout (`.nth/worktrees`) would read the
    // same files again from there.
    let main = root.as_deref().and_then(main_checkout);
    let above = above
        .iter()
        .filter(|dir| main.as_ref().is_none_or(|main| !dir.starts_with(main)));
    let (mut files, name) = NAMES
        .iter()
        .find_map(|name| {
            let files: Vec<PathBuf> = inside
                .iter()
                .map(|dir| dir.join(name))
                .filter(|path| path.is_file())
                .collect();
            (!files.is_empty()).then_some((files, Some(*name)))
        })
        .unwrap_or_default();
    files.extend(above.filter_map(|dir| first(dir, &NAMES)));
    files.reverse();
    (files, name)
}

/// The main checkout of the linked worktree at `root`, whose `.git` is a
/// file naming `<main>/.git/worktrees/<name>`.
fn main_checkout(root: &Path) -> Option<PathBuf> {
    let link = std::fs::read_to_string(root.join(".git")).ok()?;
    let gitdir = normalize(&root.join(link.strip_prefix("gitdir:")?.trim()));
    let worktrees = gitdir.parent()?;
    let git = worktrees.parent()?;
    let linked = worktrees.file_name()? == "worktrees" && git.file_name()? == ".git";
    linked.then(|| git.parent().map(Path::to_path_buf))?
}

/// The first of `names` that is a file in `dir`.
fn first(dir: &Path, names: &[&str]) -> Option<PathBuf> {
    names
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.is_file())
}

/// The instruction files from `dir` up to the project root that are not in
/// `loaded` yet, root first, claiming each in `loaded`. `dir` is the folder
/// of a file read, or a directory listed, whose own file then counts. Only
/// files called `name` count when the project has one (the name `discover`
/// settled on); without one the first name found in each directory does.
/// The root's own files are already in the system prompt, so the walk stops
/// below it. Each file comes with its imports, claimed in `loaded` too.
pub async fn nested(
    dir: PathBuf,
    cwd: PathBuf,
    name: Option<&'static str>,
    loaded: Arc<Mutex<BTreeSet<PathBuf>>>,
) -> Vec<Instruction> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    crate::blocking(move || nested_blocking(&dir, &cwd, name, home.as_deref(), &loaded)).await
}

fn nested_blocking(
    dir: &Path,
    cwd: &Path,
    name: Option<&'static str>,
    home: Option<&Path>,
    loaded: &Mutex<BTreeSet<PathBuf>>,
) -> Vec<Instruction> {
    let cwd = normalize(cwd);
    let dir = normalize(&cwd.join(dir));
    let root = project_root(&cwd).unwrap_or(cwd);
    let names: &[&str] = match &name {
        Some(name) => std::slice::from_ref(name),
        None => &NAMES,
    };
    let mut found: Vec<PathBuf> = dir
        .ancestors()
        .take_while(|dir| *dir != root && dir.starts_with(&root))
        .filter_map(|dir| first(dir, names))
        .filter(|path| claim(loaded, path))
        .collect();
    found.reverse();
    let mut ignored = Vec::new();
    let mut instructions = Vec::new();
    for path in found {
        let Some(instruction) = read(path, &mut ignored) else {
            continue;
        };
        let imported = imports::expand(
            &instruction,
            home,
            &mut |path| claim(loaded, path),
            &mut ignored,
        );
        instructions.push(instruction);
        instructions.extend(imported);
    }
    instructions
}

/// Takes `path` for this read; false when another has it. Claimed before
/// reading, so a parallel read never takes it too.
fn claim(loaded: &Mutex<BTreeSet<PathBuf>>, path: &Path) -> bool {
    loaded
        .lock()
        .expect("only poisoned if a holder panicked")
        .insert(path.to_path_buf())
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

    fn home(tree: &Tree) -> Paths {
        Paths {
            home: Some(tree.path("home")),
            ..Paths::default()
        }
    }

    #[test]
    fn outside_a_repository_the_walk_reaches_home() {
        let tree = Tree::new();
        tree.file("AGENTS.md", "above home")
            .file("home/AGENTS.md", "home")
            .file("home/notes/CLAUDE.md", "notes")
            .file("home/notes/day/AGENTS.md", "day");

        assert_eq!(
            tree.discover("home/notes/day", &home(&tree)),
            pairs(&[
                ("home/AGENTS.md", "home"),
                ("home/notes/CLAUDE.md", "notes"),
                ("home/notes/day/AGENTS.md", "day"),
            ])
        );
    }

    #[test]
    fn under_home_the_walk_passes_the_project_root() {
        let tree = Tree::new();
        tree.file("home/AGENTS.md", "home")
            .file("home/repos/AGENTS.md", "repos")
            .file("home/repos/repo/.git/HEAD", "")
            .file("home/repos/repo/CLAUDE.md", "root")
            .file("home/repos/repo/sub/CLAUDE.md", "sub");

        assert_eq!(
            tree.discover("home/repos/repo/sub", &home(&tree)),
            pairs(&[
                ("home/AGENTS.md", "home"),
                ("home/repos/AGENTS.md", "repos"),
                ("home/repos/repo/CLAUDE.md", "root"),
                ("home/repos/repo/sub/CLAUDE.md", "sub"),
            ])
        );
    }

    #[test]
    fn outside_home_the_walk_stops_at_the_project_root() {
        let tree = Tree::new();
        tree.file("srv/AGENTS.md", "above the repo")
            .file("srv/repo/.git/HEAD", "")
            .file("srv/repo/AGENTS.md", "root");

        assert_eq!(
            tree.discover("srv/repo", &home(&tree)),
            pairs(&[("srv/repo/AGENTS.md", "root")])
        );
    }

    #[test]
    fn a_worktree_skips_its_main_checkout() {
        let tree = Tree::new();
        let gitdir = tree.path("home/repo/.git/worktrees/x");
        tree.file("home/AGENTS.md", "home")
            .file("home/repo/.git/HEAD", "")
            .file("home/repo/CLAUDE.md", "main")
            .file(
                "home/repo/.nth/worktrees/x/.git",
                &format!("gitdir: {}\n", gitdir.display()),
            )
            .file("home/repo/.nth/worktrees/x/CLAUDE.md", "worktree");

        assert_eq!(
            tree.discover("home/repo/.nth/worktrees/x", &home(&tree)),
            pairs(&[
                ("home/AGENTS.md", "home"),
                ("home/repo/.nth/worktrees/x/CLAUDE.md", "worktree"),
            ])
        );
    }

    #[test]
    fn a_global_file_the_walk_reaches_is_read_once() {
        let tree = Tree::new();
        tree.file("home/.config/nth/AGENTS.md", "nth");
        let paths = Paths {
            config_home: Some(tree.path("home/.config")),
            ..home(&tree)
        };

        assert_eq!(
            tree.discover("home/.config/nth", &paths),
            pairs(&[("home/.config/nth/AGENTS.md", "nth")])
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
        dir: &str,
        loaded: &Arc<Mutex<BTreeSet<PathBuf>>>,
    ) -> Vec<(String, String)> {
        nested_as(tree, dir, None, loaded).await
    }

    async fn nested_as(
        tree: &Tree,
        dir: &str,
        name: Option<&'static str>,
        loaded: &Arc<Mutex<BTreeSet<PathBuf>>>,
    ) -> Vec<(String, String)> {
        nested(dir.into(), tree.path("repo"), name, loaded.clone())
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
            .file("repo/a/b/x.rs", "");
        let loaded = loaded(&[tree.path("repo/AGENTS.md")]);

        assert_eq!(
            nested_in(&tree, "a/b", &loaded).await,
            pairs(&[("repo/a/AGENTS.md", "a"), ("repo/a/b/CLAUDE.md", "b")])
        );
        assert_eq!(nested_in(&tree, "./a/b/../b", &loaded).await, pairs(&[]));
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
            nested_as(&tree, "crates/a/b", name, &loaded).await,
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

        assert_eq!(nested_in(&tree, ".", &loaded).await, pairs(&[]));
        let outside = tree.path("elsewhere").display().to_string();
        assert_eq!(nested_in(&tree, &outside, &loaded).await, pairs(&[]));
    }

    #[tokio::test]
    async fn parallel_reads_attach_a_file_once() {
        let tree = Tree::new();
        tree.file("repo/.git/HEAD", "")
            .file("repo/a/AGENTS.md", "a")
            .file("repo/a/x.rs", "");
        let loaded = loaded(&[]);

        let (x, y) = tokio::join!(
            nested_in(&tree, "a", &loaded),
            nested_in(&tree, "a", &loaded)
        );

        assert_eq!(x.len() + y.len(), 1, "{x:?} {y:?}");
    }

    #[test]
    fn imports_follow_the_file_that_names_them() {
        let tree = Tree::new();
        tree.file("repo/.git/HEAD", "")
            .file("repo/AGENTS.md", "root @docs/style.md and @agents\nmore")
            .file("repo/docs/style.md", "style @../RULES.md")
            .file("repo/RULES.md", "rules")
            .file("repo/sub/AGENTS.md", "sub");

        assert_eq!(
            tree.discover("repo/sub", &no_globals()),
            pairs(&[
                ("repo/AGENTS.md", "root @docs/style.md and @agents\nmore"),
                ("repo/docs/style.md", "style @../RULES.md"),
                ("repo/RULES.md", "rules"),
                ("repo/sub/AGENTS.md", "sub"),
            ])
        );
    }

    #[test]
    fn a_tilde_import_is_read_from_home() {
        let tree = Tree::new();
        tree.file("home/.claude/CLAUDE.md", "@RTK.md\n@~/notes/me.md")
            .file("home/.claude/RTK.md", "rtk")
            .file("home/notes/me.md", "me");
        let paths = Paths {
            config_home: Some(tree.path("home/.config")),
            ..home(&tree)
        };

        assert_eq!(
            tree.discover("home", &paths),
            pairs(&[
                ("home/.claude/CLAUDE.md", "@RTK.md\n@~/notes/me.md"),
                ("home/.claude/RTK.md", "rtk"),
                ("home/notes/me.md", "me"),
            ])
        );
    }

    #[test]
    fn imports_stop_four_files_deep() {
        let tree = Tree::new();
        tree.file("repo/.git/HEAD", "")
            .file("repo/AGENTS.md", "@1.md")
            .file("repo/1.md", "@2.md")
            .file("repo/2.md", "@3.md")
            .file("repo/3.md", "@4.md")
            .file("repo/4.md", "@5.md")
            .file("repo/5.md", "too deep");

        let found: Vec<String> = tree
            .discover("repo", &no_globals())
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        assert_eq!(
            found,
            [
                "repo/AGENTS.md",
                "repo/1.md",
                "repo/2.md",
                "repo/3.md",
                "repo/4.md"
            ]
        );
    }

    #[test]
    fn a_file_comes_once_however_often_it_is_named() {
        let tree = Tree::new();
        tree.file("repo/.git/HEAD", "")
            .file("repo/AGENTS.md", "@a.md @b.md @AGENTS.md")
            .file("repo/a.md", "@b.md")
            .file("repo/b.md", "@a.md");

        assert_eq!(
            tree.discover("repo", &no_globals()),
            pairs(&[
                ("repo/AGENTS.md", "@a.md @b.md @AGENTS.md"),
                ("repo/a.md", "@b.md"),
                ("repo/b.md", "@a.md"),
            ])
        );
    }

    #[tokio::test]
    async fn a_nested_file_brings_its_imports_once() {
        let tree = Tree::new();
        tree.file("repo/.git/HEAD", "")
            .file("repo/a/AGENTS.md", "a @notes.md")
            .file("repo/a/notes.md", "notes")
            .file("repo/a/b/AGENTS.md", "b @../notes.md")
            .file("repo/a/b/x.rs", "");
        let loaded = loaded(&[]);

        assert_eq!(
            nested_in(&tree, "a/b", &loaded).await,
            pairs(&[
                ("repo/a/AGENTS.md", "a @notes.md"),
                ("repo/a/notes.md", "notes"),
                ("repo/a/b/AGENTS.md", "b @../notes.md"),
            ])
        );
    }

    #[tokio::test]
    async fn parallel_reads_attach_an_import_once() {
        let tree = Tree::new();
        tree.file("repo/.git/HEAD", "")
            .file("repo/a/AGENTS.md", "@../shared.md")
            .file("repo/b/AGENTS.md", "@../shared.md")
            .file("repo/shared.md", "shared");
        let loaded = loaded(&[]);

        let (a, b) = tokio::join!(
            nested_in(&tree, "a", &loaded),
            nested_in(&tree, "b", &loaded)
        );

        let shared = a.iter().chain(&b).filter(|(p, _)| p == "repo/shared.md");
        assert_eq!(shared.count(), 1, "{a:?} {b:?}");
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
