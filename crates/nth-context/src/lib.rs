//! What nth learns about a project before the first prompt: the instruction
//! files (`AGENTS.md`, or `CLAUDE.md` where there is none) that go into the
//! system prompt, and the skills the model and the user can call on.

pub mod instructions;
pub mod skills;

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

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
    /// The name the project's instruction files go by, `AGENTS.md` or
    /// `CLAUDE.md`, once one was found: the files the read tool attaches
    /// later keep to it, so a project with both is not read twice.
    pub instruction_name: Option<&'static str>,
    pub skills: Skills,
    /// Files that were found but could not be used, worth telling the user.
    pub warnings: Vec<String>,
}

impl Context {
    /// Reads what applies to `cwd`. Blocking file IO; from async code use
    /// [`Context::load`].
    pub fn discover(cwd: &Path, paths: &Paths) -> Self {
        let mut context = Self::default();
        let (instructions, name) = instructions::discover(cwd, paths, &mut context.warnings);
        context.instructions = instructions;
        context.instruction_name = name;
        context.skills = skills::discover(cwd, paths, &mut context.warnings);
        context
    }

    /// [`Context::discover`] off the async runtime.
    pub async fn load(cwd: PathBuf, paths: Paths) -> Self {
        blocking(move || Self::discover(&cwd, &paths)).await
    }
}

/// Runs file system work off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(f).await {
        Ok(value) => value,
        // Only a panic in `f` gets here; pass it on as it was.
        Err(e) => std::panic::resume_unwind(e.into_panic()),
    }
}

/// The nearest directory at or above `cwd` that holds `.git`, a directory in
/// a clone and a file in a worktree.
pub fn project_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// A program by name on `PATH`, or as given when it is a path already.
/// Blocking.
pub fn which(program: &str) -> Option<PathBuf> {
    which_in(program, std::env::var_os("PATH").as_deref())
}

/// [`which`] over `search_path`, shaped like `PATH`, instead of `PATH`
/// itself. Blocking.
pub fn which_in(program: &str, search_path: Option<&OsStr>) -> Option<PathBuf> {
    if program.contains('/') {
        let path = PathBuf::from(program);
        return executable(&path).then_some(path);
    }
    std::env::split_paths(search_path?)
        .map(|dir| dir.join(program))
        .find(|path| executable(path))
}

fn executable(path: &Path) -> bool {
    match path.metadata() {
        #[cfg(unix)]
        Ok(meta) => {
            use std::os::unix::fs::PermissionsExt;
            meta.is_file() && meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        Ok(meta) => meta.is_file(),
        Err(_) => false,
    }
}

/// The keys a file is looked up by in a table of extensions: every dotted
/// suffix of its name, longest first, so `x.html.erb` tries `.html.erb`
/// before `.erb`; the whole name for a file without a dot (`makefile`,
/// `Dockerfile`). Empty for a path without a file name.
pub fn extension_keys(path: &Path) -> Vec<String> {
    let Some(name) = path.file_name() else {
        return Vec::new();
    };
    let name = name.to_string_lossy();
    let keys: Vec<String> = name
        .match_indices('.')
        .map(|(at, _)| name[at..].to_string())
        .collect();
    match keys.is_empty() {
        true => vec![name.into_owned()],
        false => keys,
    }
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

    #[test]
    fn which_needs_an_executable() {
        assert!(which("sh").is_some());
        assert!(which("definitely-not-a-program-nth").is_none());
        let tmp = tempfile::tempdir().expect("tempdir");
        let plain = tmp.path().join("plain");
        std::fs::write(&plain, "").expect("write");
        assert!(which(plain.to_str().expect("utf-8")).is_none());
        assert!(which_in("sh", Some(tmp.path().as_os_str())).is_none());
    }

    #[test]
    fn extension_keys_are_every_dotted_suffix() {
        let keys = |p: &str| extension_keys(Path::new(p));
        assert_eq!(keys("/a/main.rs"), [".rs"]);
        assert_eq!(keys("/a/view.html.erb"), [".html.erb", ".erb"]);
        assert_eq!(keys("/a/.eslintrc.json"), [".eslintrc.json", ".json"]);
        assert_eq!(keys("/a/Dockerfile"), ["Dockerfile"]);
        assert_eq!(keys("/a/.gitignore"), [".gitignore"]);
        assert!(keys("/").is_empty());
    }
}
