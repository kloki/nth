//! What nth learns about a project before the first prompt: the instruction
//! files (`AGENTS.md`, or `CLAUDE.md` where there is none) that go into the
//! system prompt, and the skills the model and the user can call on.

pub mod instructions;
pub mod skills;

use std::path::{Path, PathBuf};

pub use instructions::Instruction;
pub use skills::{Skill, Skills};

/// Where the global files live. Read from the environment once, and passed
/// in so tests never see the real home directory.
#[derive(Debug, Clone, Default)]
pub struct Paths {
    pub home: Option<PathBuf>,
    /// `$XDG_CONFIG_HOME`, or `~/.config` when that is unset. nth's own
    /// files are under `nth/` in it, opencode's under `opencode/`.
    pub config_home: Option<PathBuf>,
    /// Extra skill folders from the config. Relative ones are resolved
    /// against the working directory.
    pub skill_paths: Vec<PathBuf>,
}

impl Paths {
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .map(PathBuf::from);
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|c| !c.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".config")));
        Self {
            home,
            config_home,
            skill_paths: Vec::new(),
        }
    }

    /// nth's own config directory.
    pub fn config_dir(&self) -> Option<PathBuf> {
        self.config_home.as_ref().map(|c| c.join("nth"))
    }
}

/// Everything found for one working directory.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Context {
    /// In the order they go into the system prompt: global first, then the
    /// project's from its root down, so the most specific comes last.
    pub instructions: Vec<Instruction>,
    pub skills: Skills,
    /// Files that were found but could not be used, worth telling the user.
    pub warnings: Vec<String>,
}

impl Context {
    /// Reads what applies to `cwd`. Blocking file IO; from async code use
    /// [`Context::load`].
    pub fn discover(cwd: &Path, paths: &Paths) -> Self {
        let mut context = Self::default();
        context.instructions = instructions::discover(cwd, paths, &mut context.warnings);
        context.skills = skills::discover(cwd, paths, &mut context.warnings);
        context
    }

    /// [`Context::discover`] off the async runtime.
    pub async fn load(cwd: PathBuf, paths: Paths) -> Self {
        match tokio::task::spawn_blocking(move || Self::discover(&cwd, &paths)).await {
            Ok(context) => context,
            // Only a panic in discover gets here; pass it on as it was.
            Err(e) => std::panic::resume_unwind(e.into_panic()),
        }
    }
}

/// The nearest directory at or above `cwd` that holds `.git`, a directory in
/// a clone and a file in a worktree.
pub fn project_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_root_is_the_nearest_git_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).expect("git dir");
        std::fs::create_dir_all(root.join("a/b")).expect("subdirs");

        assert_eq!(project_root(&root.join("a/b")), Some(root.to_path_buf()));
        assert_eq!(project_root(root), Some(root.to_path_buf()));
    }

    #[test]
    fn config_dir_is_under_config_home() {
        let paths = Paths {
            home: Some("/home/k".into()),
            config_home: Some("/home/k/.config".into()),
            ..Paths::default()
        };

        assert_eq!(paths.config_dir(), Some("/home/k/.config/nth".into()));
    }
}
