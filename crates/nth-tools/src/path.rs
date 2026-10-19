//! Where a path the model named points. Models write `~/notes.md` as a
//! shell would, so `~` is the home directory rather than a folder named `~`.

use std::path::{Path, PathBuf};

/// `raw` resolved against `cwd`, with a leading `~` read from `$HOME`.
pub(crate) fn resolve(cwd: &Path, raw: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_in(cwd, raw, home.as_deref())
}

/// `raw` resolved against `cwd`, with exactly `~` or a leading `~/` meaning
/// `home`. `~user` and a `~` further in are names like any other, and with
/// no home `~` stays one too.
pub(crate) fn resolve_in(cwd: &Path, raw: &str, home: Option<&Path>) -> PathBuf {
    nth_context::expand_home(raw, home).unwrap_or_else(|| cwd.join(raw))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CWD: &str = "/work";
    const HOME: &str = "/home/me";

    fn resolve(raw: &str) -> PathBuf {
        resolve_in(Path::new(CWD), raw, Some(Path::new(HOME)))
    }

    #[test]
    fn tilde_is_home() {
        assert_eq!(resolve("~"), PathBuf::from(HOME));
        assert_eq!(resolve("~/notes.md"), PathBuf::from("/home/me/notes.md"));
        assert_eq!(resolve("~/a/b"), PathBuf::from("/home/me/a/b"));
    }

    #[test]
    fn other_tildes_are_names() {
        assert_eq!(resolve("~user/x"), PathBuf::from("/work/~user/x"));
        assert_eq!(resolve("a/~"), PathBuf::from("/work/a/~"));
        assert_eq!(resolve("a/~/b"), PathBuf::from("/work/a/~/b"));
    }

    #[test]
    fn plain_paths_join_the_working_directory() {
        assert_eq!(resolve("src/lib.rs"), PathBuf::from("/work/src/lib.rs"));
        assert_eq!(resolve("/etc/hosts"), PathBuf::from("/etc/hosts"));
    }

    #[test]
    fn without_a_home_tilde_stays() {
        let resolved = resolve_in(Path::new(CWD), "~/x", None);
        assert_eq!(resolved, PathBuf::from("/work/~/x"));
    }
}
