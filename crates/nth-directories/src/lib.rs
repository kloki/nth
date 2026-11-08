//! Repository discovery and the locations of project-local nth state.

use std::path::{Component, Path, PathBuf};

/// The nearest checkout, including a submodule or linked worktree.
pub fn project_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The main checkout of the nearest repository, if there is one.
pub fn main_project_root(cwd: &Path) -> Option<PathBuf> {
    let root = project_root(cwd)?;
    Some(main_checkout(&root).unwrap_or(root))
}

/// The main checkout if `root` is a linked worktree. A submodule's gitfile
/// alone does not make it a linked worktree.
pub fn main_checkout(root: &Path) -> Option<PathBuf> {
    let link = std::fs::read_to_string(root.join(".git")).ok()?;
    let gitdir = normalize(&root.join(link.strip_prefix("gitdir:")?.trim()));
    let worktrees = gitdir.parent()?;
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let common = worktrees.parent()?;
    if common.file_name()? == ".git" {
        return common.parent().map(Path::to_path_buf);
    }
    // A submodule's common git directory is under the superproject's
    // .git/modules; Git records the submodule checkout relative to it.
    let output = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(common)
        .args(["config", "--path", "--get", "core.worktree"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let worktree = String::from_utf8(output.stdout).ok()?;
    let path = normalize(&common.join(worktree.trim()));
    // Only accept a checkout that points back to this common git directory.
    let link = std::fs::read_to_string(path.join(".git")).ok()?;
    (normalize(&path.join(link.strip_prefix("gitdir:")?.trim())) == common).then_some(path)
}

fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir if result.file_name().is_some_and(|n| n != "..") => {
                result.pop();
            }
            Component::ParentDir if !result.has_root() => result.push(part),
            Component::ParentDir => {}
            _ => result.push(part),
        }
    }
    result
}

/// Locations for one working directory. Outside Git, both roots are `cwd`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Directories {
    pub checkout: PathBuf,
    pub main_checkout: PathBuf,
}

impl Directories {
    /// Use a previously resolved checkout root without consulting the filesystem.
    /// Both checkout fields are set to `root`, which contains the `.nth` directory.
    pub fn from_root(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        Self {
            checkout: root.clone(),
            main_checkout: root,
        }
    }

    pub fn discover(cwd: &Path) -> Self {
        let checkout = project_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
        let main_checkout = main_checkout(&checkout).unwrap_or_else(|| checkout.clone());
        Self {
            checkout,
            main_checkout,
        }
    }

    /// State shared by the repository's worktrees.
    pub fn nth(&self) -> PathBuf {
        self.main_checkout.join(".nth")
    }
    pub fn plans(&self) -> PathBuf {
        self.nth().join("plans")
    }
    /// The plan file for a session.
    pub fn plan(&self, session: &str) -> PathBuf {
        self.plans().join(format!("{session}.md"))
    }
    pub fn work(&self) -> PathBuf {
        self.nth().join("work")
    }
    /// The work directory for a session.
    pub fn session_work(&self, session: &str) -> PathBuf {
        self.work().join(session)
    }
    pub fn memory(&self) -> PathBuf {
        self.nth().join("memory")
    }
    pub fn curios(&self) -> PathBuf {
        self.nth().join("curios")
    }
    /// The Curios workspace belonging to an owner session.
    pub fn curio_owner(&self, session: &str) -> PathBuf {
        self.curios().join(session)
    }
    /// The editable source directory for an owner session.
    pub fn curio_source(&self, session: &str) -> PathBuf {
        self.curio_owner(session).join("source")
    }
    pub fn worktrees(&self) -> PathBuf {
        self.nth().join("worktrees")
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "-q"]);
        git(
            dir,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
        );
    }

    #[test]
    fn checkout_and_shared_paths_for_real_worktrees() {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        let linked = temp.path().join("linked");
        init(&repo);
        git(
            &repo,
            &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
        );
        let dirs = Directories::discover(&linked);
        assert_eq!(dirs.checkout, linked);
        assert_eq!(dirs.main_checkout, repo);
        for (path, name) in [
            (dirs.plans(), "plans"),
            (dirs.work(), "work"),
            (dirs.memory(), "memory"),
            (dirs.curios(), "curios"),
            (dirs.worktrees(), "worktrees"),
        ] {
            assert_eq!(path, repo.join(".nth").join(name));
        }
        assert_eq!(main_project_root(&linked), Some(repo));
    }

    #[test]
    fn submodule_and_its_worktree_are_a_distinct_repository() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let repo = temp.path().join("repo");
        let linked = temp.path().join("sub-linked");
        init(&source);
        init(&repo);
        git(
            &repo,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                source.to_str().unwrap(),
                "sub",
            ],
        );
        let sub = repo.join("sub");
        assert_eq!(Directories::discover(&sub).main_checkout, sub);
        git(
            &sub,
            &["worktree", "add", "-qb", "linked", linked.to_str().unwrap()],
        );
        let dirs = Directories::discover(&linked);
        assert_eq!(dirs.checkout, linked);
        assert_eq!(dirs.main_checkout, sub);
        assert_eq!(dirs.memory(), sub.join(".nth/memory"));
    }

    #[test]
    fn outside_git_uses_the_given_directory() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = Directories::discover(temp.path());
        assert_eq!(project_root(temp.path()), None);
        assert_eq!(dirs.checkout, temp.path());
        assert_eq!(dirs.main_checkout, temp.path());
        assert_eq!(dirs.memory(), temp.path().join(".nth/memory"));
    }

    #[test]
    fn persisted_root_builds_session_paths_without_discovery() {
        let root = PathBuf::from("/known/root");
        let dirs = Directories::from_root(&root);
        assert_eq!(dirs.checkout, root);
        assert_eq!(dirs.main_checkout, root);
        assert_eq!(dirs.plan("abc"), root.join(".nth/plans/abc.md"));
        assert_eq!(dirs.session_work("abc"), root.join(".nth/work/abc"));
        assert_eq!(dirs.curio_owner("abc"), root.join(".nth/curios/abc"));
        assert_eq!(
            dirs.curio_source("abc"),
            root.join(".nth/curios/abc/source")
        );
    }
}
