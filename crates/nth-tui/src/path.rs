//! Paths as typed into the prompt: expanded to what they name, and
//! completed by listing the directory being typed, for `/add-dir` and for
//! an `@` mention that leaves the working directory.

use std::path::{Path, PathBuf};

use crate::mention;

/// `arg` as the directory it names: `~` or `~/` for home, an absolute
/// path as it is, anything else against `cwd`.
pub fn expand(cwd: &Path, home: Option<&str>, arg: &str) -> PathBuf {
    let home = home.filter(|home| !home.is_empty()).map(Path::new);
    if let Some(path) = nth_context::expand_home(arg, home) {
        return path;
    }
    let path = Path::new(arg);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// What [`candidates`] lists.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Entries {
    /// Only directories, hidden ones too: what `/add-dir` takes.
    Dirs,
    /// Files and directories, the hidden ones only once the name typed
    /// starts with `.`: what a mention names.
    Visible,
}

/// The entries that complete `arg`: those of the directory it names — the
/// working directory when it names none — each spelled the way `arg`
/// spells its directory, so accepting one only ever extends what is typed.
/// A directory ends in `/` so the next keystroke lists the one it names.
pub fn candidates(cwd: &Path, home: Option<&str>, arg: &str, entries: Entries) -> Vec<String> {
    let (dir, prefix, part) = parts(cwd, home, arg);
    let Ok(listing) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let hidden = entries == Entries::Dirs || part.starts_with('.');
    let mut names: Vec<String> = listing
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            // `is_dir` follows a symlink, as `/add-dir` does when it adds one.
            let is_dir = entry.path().is_dir();
            let listed = name.starts_with(part)
                && (hidden || !name.starts_with('.'))
                && (is_dir || entries == Entries::Visible);
            listed.then(|| if is_dir { format!("{name}/") } else { name })
        })
        .collect();
    names.sort();
    names.truncate(mention::LIMIT);
    names
        .into_iter()
        .map(|name| format!("{prefix}{name}"))
        .collect()
}

/// The directory `arg` names for [`candidates`](self) to list, how to spell
/// it in the prompt, and the part of the name to match: everything typed up
/// to the last `/` is the directory, everything after it the name.
fn parts<'a>(cwd: &Path, home: Option<&str>, arg: &'a str) -> (PathBuf, String, &'a str) {
    match arg.rfind('/') {
        Some(at) => {
            let (spelled, name) = arg.split_at(at);
            // An absolute argument names its directory from the root; `""`,
            // the part of `/` itself, is the filesystem's own.
            let dir = if arg.starts_with('/') {
                Path::new("/").join(spelled)
            } else {
                expand(cwd, home, spelled)
            };
            (dir, format!("{spelled}/"), &name[1..])
        }
        // `~` and what follows it name home; anything else names `cwd`.
        None => match (arg.strip_prefix('~'), home.filter(|h| !h.is_empty())) {
            (Some(name), Some(home)) => (PathBuf::from(home), "~/".into(), name),
            _ => (cwd.to_path_buf(), String::new(), arg),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(base: &Path, name: &str) -> PathBuf {
        let dir = base.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn expands_home_absolute_and_relative_arguments() {
        let cwd = Path::new("/repo");

        assert_eq!(expand(cwd, Some("/home/k"), "~"), PathBuf::from("/home/k"));
        assert_eq!(
            expand(cwd, Some("/home/k"), "~/other"),
            PathBuf::from("/home/k/other")
        );
        assert_eq!(expand(cwd, None, "~"), PathBuf::from("/repo/~"));
        assert_eq!(expand(cwd, Some("/home/k"), "other"), cwd.join("other"));
        assert_eq!(expand(cwd, Some("/home/k"), "/abs"), PathBuf::from("/abs"));
    }

    #[test]
    fn a_tilde_before_a_name_is_part_of_the_name() {
        let cwd = Path::new("/repo");

        assert_eq!(
            expand(cwd, Some("/home/k"), "~other"),
            PathBuf::from("/repo/~other")
        );
    }

    #[test]
    fn lists_the_subdirectories_of_the_one_named() {
        let base = tempfile::tempdir().expect("tempdir");
        let base = base.path();
        dir(base, "src");
        dir(base, "docs");
        std::fs::write(base.join("Cargo.toml"), "").expect("file");
        let cwd = base.to_path_buf();

        assert_eq!(
            candidates(&cwd, None, "", Entries::Dirs),
            ["docs/", "src/"],
            "the working directory's own, files left out"
        );
        assert_eq!(candidates(&cwd, None, "s", Entries::Dirs), ["src/"]);
        assert_eq!(
            candidates(&cwd, None, "zzz", Entries::Dirs),
            Vec::<String>::new()
        );

        let src = dir(base, "src");
        dir(&src, "app");
        assert_eq!(candidates(&cwd, None, "src", Entries::Dirs), ["src/"]);
        assert_eq!(candidates(&cwd, None, "src/", Entries::Dirs), ["src/app/"]);
        assert_eq!(candidates(&cwd, None, "src/a", Entries::Dirs), ["src/app/"]);

        // An absolute argument spells its candidates absolutely.
        let abs = src.display().to_string();
        assert_eq!(
            candidates(&cwd, None, &format!("{abs}/a"), Entries::Dirs),
            [format!("{abs}/app/")]
        );
        let roots = candidates(&cwd, None, "/", Entries::Dirs);
        assert!(!roots.is_empty(), "the filesystem's own directories");
        assert!(roots.iter().all(|name| {
            let dir = &name[..name.len() - 1];
            name.ends_with('/') && Path::new(dir).is_dir()
        }));
    }

    #[test]
    fn visible_lists_files_too_and_hidden_ones_once_a_dot_is_typed() {
        let base = tempfile::tempdir().expect("tempdir");
        let base = base.path();
        let cwd = dir(base, "repo");
        dir(base, "other");
        std::fs::write(base.join("notes.md"), "").expect("file");
        std::fs::write(base.join(".env"), "").expect("file");

        assert_eq!(
            candidates(&cwd, None, "../", Entries::Visible),
            ["../notes.md", "../other/", "../repo/"]
        );
        assert_eq!(
            candidates(&cwd, None, "../.", Entries::Visible),
            ["../.env"]
        );
        assert_eq!(
            candidates(&cwd, None, "../", Entries::Dirs),
            ["../other/", "../repo/"]
        );
    }

    #[test]
    fn tilde_lists_home_and_spells_it() {
        let base = tempfile::tempdir().expect("tempdir");
        let home = dir(base.path(), "home");
        dir(&home, "repos");
        let home = home.display().to_string();

        assert_eq!(
            candidates(Path::new("/repo"), Some(&home), "~", Entries::Dirs),
            ["~/repos/"]
        );
        assert_eq!(
            candidates(Path::new("/repo"), Some(&home), "~/rep", Entries::Dirs),
            ["~/repos/"]
        );
    }

    #[test]
    fn a_missing_directory_lists_nothing() {
        assert!(candidates(Path::new("/nowhere"), None, "", Entries::Dirs).is_empty());
        assert!(candidates(Path::new("/nowhere"), None, "no/pe", Entries::Dirs).is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_to_a_directory_is_listed() {
        let base = tempfile::tempdir().expect("tempdir");
        let target = dir(base.path(), "data/work");
        let cwd = dir(base.path(), "cwd");
        std::os::unix::fs::symlink(&target, cwd.join("work")).expect("symlink");
        std::fs::write(target.join("file"), "").expect("file");
        std::os::unix::fs::symlink(target.join("file"), cwd.join("link")).expect("symlink");

        assert_eq!(candidates(&cwd, None, "", Entries::Dirs), ["work/"]);
    }
}
