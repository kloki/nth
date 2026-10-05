//! Which files the tools may write. Plan mode allows only the plan file;
//! the tools that write files check here before they touch one.

use std::path::{Component, Path, PathBuf};

/// Binds only the tools that write files: write, edit and apply_patch.
/// bash and monitor do not consult it, so a shell command could still
/// change a file in plan mode. That is left to the prompt, as opencode
/// leaves it: the plan reminder forbids file-changing commands and allows
/// only ones that read. Telling a read-only `grep` from a `sed -i` means
/// parsing shell, an endless list of exceptions that would refuse honest
/// commands and still miss some that write.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Writable {
    #[default]
    Any,
    /// Only this file, an absolute path.
    Only(PathBuf),
}

impl Writable {
    /// `Err` with a message for the model when `path`, absolute as the
    /// tools resolve it, may not be written.
    pub fn check(&self, path: &Path) -> Result<(), String> {
        match self {
            Writable::Any => Ok(()),
            Writable::Only(allowed) if normalize(path) == normalize(allowed) => Ok(()),
            Writable::Only(allowed) => Err(format!(
                "plan mode is active: only the plan file {} may be written; {} is read-only \
                 until the user approves the plan with /approve",
                allowed.display(),
                path.display()
            )),
        }
    }
}

/// Folds `.` and `..` without touching the disk: the plan file and its
/// folder may not exist yet, so the path can't be canonicalized.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_allowed_file_passes() {
        let only = Writable::Only("/repo/.nth/plans/1.md".into());

        assert_eq!(only.check(Path::new("/repo/.nth/plans/1.md")), Ok(()));
        assert_eq!(
            only.check(Path::new("/repo/./.nth/x/../plans/1.md")),
            Ok(())
        );
        let err = only
            .check(Path::new("/repo/src/main.rs"))
            .expect_err("refused");
        assert!(err.contains("/repo/.nth/plans/1.md"), "{err}");
        assert!(err.contains("/approve"), "{err}");
        assert!(
            only.check(Path::new("/repo/.nth/plans/1.md/../2.md"))
                .is_err()
        );
        assert_eq!(Writable::Any.check(Path::new("/anything")), Ok(()));
    }
}
