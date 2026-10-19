//! Which files the tools may write. Plan mode allows only the plan file
//! and the project's memory; the tools that write files check here before
//! they touch one.

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
    /// Plan mode: only the plan file, an absolute path, and what is in the
    /// memory folder when the session keeps one, since what the model
    /// learns while planning is worth keeping too.
    Plan {
        file: PathBuf,
        memory: Option<PathBuf>,
    },
}

impl Writable {
    /// Plan mode with no memory folder.
    pub fn plan(file: PathBuf) -> Self {
        Writable::Plan { file, memory: None }
    }

    /// Whether this is plan mode, which some tools refuse in.
    pub fn planning(&self) -> bool {
        matches!(self, Writable::Plan { .. })
    }

    /// `Err` with a message for the model when `path`, absolute as the
    /// tools resolve it, may not be written.
    pub fn check(&self, path: &Path) -> Result<(), String> {
        let Writable::Plan { file, memory } = self else {
            return Ok(());
        };
        let path = normalize(path);
        if path == normalize(file) {
            return Ok(());
        }
        if let Some(memory) = memory
            && path.starts_with(normalize(memory))
        {
            return Ok(());
        }
        let memory = match memory {
            Some(memory) => format!(" and the memory folder {}", memory.display()),
            None => String::new(),
        };
        Err(format!(
            "plan mode is active: only the plan file {}{memory} may be written; {} is \
             read-only until the user approves the plan with /approve",
            file.display(),
            path.display()
        ))
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
        let only = Writable::plan("/repo/.nth/plans/1.md".into());

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

    #[test]
    fn the_memory_folder_passes_too() {
        let plan = Writable::Plan {
            file: "/repo/.nth/plans/1.md".into(),
            memory: Some("/repo/.nth/memory".into()),
        };

        assert_eq!(plan.check(Path::new("/repo/.nth/memory/MEMORY.md")), Ok(()));
        assert_eq!(plan.check(Path::new("/repo/.nth/memory/a/b.md")), Ok(()));
        let err = plan
            .check(Path::new("/repo/.nth/memory/../work/x.md"))
            .expect_err("refused");
        assert!(err.contains("the memory folder /repo/.nth/memory"), "{err}");
        assert!(plan.check(Path::new("/repo/.nth/memory-x/a.md")).is_err());
        assert!(plan.planning());
        assert!(!Writable::Any.planning());
    }
}
