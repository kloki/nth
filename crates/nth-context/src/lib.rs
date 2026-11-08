//! What nth learns about a project before the first prompt: the instruction
//! files (`AGENTS.md`, or `CLAUDE.md` where there is none) that go into the
//! system prompt, the skills the model and the user can call on, and the
//! agents the model can delegate to.

pub mod agents;
pub mod frontmatter;
pub mod instructions;
pub mod memory;
pub mod skills;

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

pub use agents::{Agent, Agents};
pub use instructions::Instruction;
pub use memory::Memory;
pub use skills::{Skill, Skills};

/// Where the global files live. Read from the environment once, and passed
/// in so tests never see the real home directory.
#[derive(Debug, Clone, Default)]
pub struct Paths {
    pub home: Option<PathBuf>,
    /// `$XDG_CONFIG_HOME`, or `~/.config` when that is unset. nth's own
    /// files are under `nth/` in it, opencode's under `opencode/`.
    pub config_home: Option<PathBuf>,
    /// `$XDG_DATA_HOME`, or `~/.local/share` when that is unset. nth keeps
    /// what it saves under `nth/` in it.
    pub data_home: Option<PathBuf>,
    /// Extra skill folders from the config. Relative ones are resolved
    /// against the working directory.
    pub skill_paths: Vec<PathBuf>,
    /// Whether the project's memory is read, from the config's `[memory]`.
    pub memory: bool,
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
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
        Self {
            home,
            config_home,
            data_home,
            skill_paths: Vec::new(),
            memory: false,
        }
    }

    /// nth's own config directory.
    pub fn config_dir(&self) -> Option<PathBuf> {
        self.config_home.as_ref().map(|c| c.join("nth"))
    }

    /// Where nth keeps what it saves: sessions, credentials, history.
    pub fn nth_data(&self) -> Option<PathBuf> {
        self.data_home.as_ref().map(|d| d.join("nth"))
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
    /// Where in the project an instruction file would have been read had
    /// it been there, so a session hears of one created later.
    pub absent_instructions: Vec<PathBuf>,
    pub skills: Skills,
    pub agents: Agents,
    /// The project's memory, when `Paths::memory` asks for it.
    pub memory: Option<Memory>,
    /// Files that were found but could not be used, worth telling the user.
    pub warnings: Vec<String>,
}

impl Context {
    /// Reads what applies to `cwd`. Blocking file IO; from async code use
    /// [`Context::load`].
    pub fn discover(cwd: &Path, paths: &Paths) -> Self {
        let mut context = Self::default();
        let found = instructions::discover(cwd, paths, &mut context.warnings);
        context.instructions = found.instructions;
        context.instruction_name = found.name;
        context.absent_instructions = found.absent;
        context.skills = skills::discover(cwd, paths, &mut context.warnings);
        context.agents = agents::discover(cwd, paths, &mut context.warnings);
        if paths.memory {
            context.memory = Some(memory::discover(cwd, &mut context.warnings));
        }
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

/// The directories from `cwd` up that project files are looked for in,
/// nearest first: up to and including the home directory when `cwd` is under
/// it, else up to the project root, else `cwd` alone.
pub fn search_dirs<'a>(cwd: &'a Path, home: Option<&Path>) -> Vec<&'a Path> {
    let root = nth_directories::project_root(cwd);
    let stop = match home {
        Some(home) if cwd.starts_with(home) => Some(home),
        _ => root.as_deref(),
    };
    match stop {
        Some(stop) => cwd
            .ancestors()
            .take_while(|dir| dir.starts_with(stop))
            .collect(),
        None => vec![cwd],
    }
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
    fn search_dirs_stop_at_home_else_the_project_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(home.join("a/b")).expect("home dirs");
        std::fs::create_dir_all(repo.join(".git")).expect("git dir");
        std::fs::create_dir_all(repo.join("sub")).expect("repo dirs");

        let under_home = home.join("a/b");
        assert_eq!(
            search_dirs(&under_home, Some(&home)),
            [under_home.as_path(), &home.join("a"), &home]
        );
        let in_repo = repo.join("sub");
        assert_eq!(
            search_dirs(&in_repo, Some(&home)),
            [in_repo.as_path(), &repo]
        );
        let outside = dir.path().join("elsewhere");
        assert_eq!(search_dirs(&outside, Some(&home)), [outside.as_path()]);
        assert_eq!(search_dirs(&outside, None), [outside.as_path()]);
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
