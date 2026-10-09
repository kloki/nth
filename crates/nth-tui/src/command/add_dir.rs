//! `/add-dir <path>`: the argument as typed, expanded to the directory it
//! names, and completed to the directories under the one being typed.

use std::path::{Path, PathBuf};

use crate::{mention, popup::Popup};

/// The argument of `/add-dir` being typed: what follows `/add-dir ` at the
/// very start of `text`. `None` for any other prompt, and for one whose
/// argument has a space in it, which no directory path has.
pub fn argument(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("/add-dir ")?;
    if rest.contains(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// The popup of directories that complete `arg`.
pub fn complete(cwd: &Path, home: Option<&str>, arg: &str) -> Option<Popup<String>> {
    Popup::new(candidates(cwd, home, arg))
}

/// `arg` as the directory it names: `~` for home, an absolute path as it
/// is, anything else against `cwd`.
pub fn expand(cwd: &Path, home: Option<&str>, arg: &str) -> PathBuf {
    if let Some(rest) = arg.strip_prefix('~')
        && let Some(home) = home.filter(|home| !home.is_empty())
    {
        return match rest.strip_prefix('/') {
            Some(rest) => Path::new(home).join(rest),
            None => PathBuf::from(home),
        };
    }
    let path = Path::new(arg);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// The directories that complete `arg`: the subdirectories of the one it
/// names — the working directory when it names none — each spelled the way
/// `arg` spells its directory, so accepting one only ever extends what is
/// typed. Only directories, each ending in `/` so the next keystroke lists
/// the one it names.
pub fn candidates(cwd: &Path, home: Option<&str>, arg: &str) -> Vec<String> {
    let (dir, prefix, part) = parts(cwd, home, arg);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(part))
        .collect();
    names.sort();
    names.truncate(mention::LIMIT);
    names
        .into_iter()
        .map(|name| format!("{prefix}{name}/"))
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
    fn finds_the_argument_only_of_add_dir() {
        assert_eq!(argument("/add-dir "), Some(""));
        assert_eq!(argument("/add-dir /tm"), Some("/tm"));
        assert_eq!(argument("/add-dir ~"), Some("~"));
        assert_eq!(argument("/add-dir  spaced"), None, "two spaces");
        assert_eq!(argument("/add-dir /tm more"), None);
        assert_eq!(argument("/add-dir"), None, "no argument yet");
        assert_eq!(argument("/exit "), None);
        assert_eq!(argument("add-dir /tm"), None);
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
    fn lists_the_subdirectories_of_the_one_named() {
        let base = tempfile::tempdir().expect("tempdir");
        let base = base.path();
        dir(base, "src");
        dir(base, "docs");
        std::fs::write(base.join("Cargo.toml"), "").expect("file");
        let cwd = base.to_path_buf();

        assert_eq!(
            candidates(&cwd, None, ""),
            ["docs/", "src/"],
            "the working directory's own, files left out"
        );
        assert_eq!(candidates(&cwd, None, "s"), ["src/"]);
        assert_eq!(candidates(&cwd, None, "zzz"), Vec::<String>::new());

        let src = dir(base, "src");
        dir(&src, "app");
        assert_eq!(candidates(&cwd, None, "src"), ["src/"]);
        assert_eq!(candidates(&cwd, None, "src/"), ["src/app/"]);
        assert_eq!(candidates(&cwd, None, "src/a"), ["src/app/"]);

        // An absolute argument spells its candidates absolutely.
        let abs = src.display().to_string();
        assert_eq!(
            candidates(&cwd, None, &format!("{abs}/a")),
            [format!("{abs}/app/")]
        );
        let roots = candidates(&cwd, None, "/");
        assert!(!roots.is_empty(), "the filesystem's own directories");
        assert!(roots.iter().all(|name| {
            let dir = &name[..name.len() - 1];
            name.ends_with('/') && Path::new(dir).is_dir()
        }));
    }

    #[test]
    fn tilde_lists_home_and_spells_it() {
        let base = tempfile::tempdir().expect("tempdir");
        let home = dir(base.path(), "home");
        dir(&home, "repos");
        let home = home.display().to_string();

        assert_eq!(
            candidates(Path::new("/repo"), Some(&home), "~"),
            ["~/repos/"]
        );
        assert_eq!(
            candidates(Path::new("/repo"), Some(&home), "~/rep"),
            ["~/repos/"]
        );
    }

    #[test]
    fn a_missing_directory_lists_nothing() {
        assert!(candidates(Path::new("/nowhere"), None, "").is_empty());
        assert!(candidates(Path::new("/nowhere"), None, "no/pe").is_empty());
    }
}
