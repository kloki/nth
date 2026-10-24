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
        source: Option<PathBuf>,
    },
}

impl Writable {
    /// Plan mode with no memory folder.
    pub fn plan(file: PathBuf) -> Self {
        Writable::Plan {
            file,
            memory: None,
            source: None,
        }
    }

    /// Whether this is plan mode, which some tools refuse in.
    pub fn planning(&self) -> bool {
        matches!(self, Writable::Plan { .. })
    }

    /// `Err` with a message for the model when `path`, absolute as the
    /// tools resolve it, may not be written.
    pub fn check(&self, path: &Path) -> Result<(), String> {
        let Writable::Plan {
            file,
            memory,
            source,
        } = self
        else {
            return Ok(());
        };
        // Only what lies inside the source folder: the folders above the
        // repository are the system's (macOS keeps temporary folders under
        // `/var`, a symlink).
        let source_safe = path
            .ancestors()
            .take_while(|p| source.as_ref().is_some_and(|s| p.starts_with(s)))
            .all(|p| match std::fs::symlink_metadata(p) {
                Ok(meta) => !meta.file_type().is_symlink(),
                Err(err) => err.kind() == std::io::ErrorKind::NotFound,
            });
        if source.as_ref().is_some_and(|s| path.starts_with(s)) && !source_safe {
            return Err("Curios source paths may not traverse symlinks".into());
        }
        let path = normalize(path);
        if let Some(source) = source
            && path.starts_with(normalize(source))
            && source_safe
        {
            return Ok(());
        }
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
            source: None,
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

    #[cfg(unix)]
    #[test]
    fn source_scope_rejects_other_sessions_traversal_and_symlinks() {
        let root = std::env::temp_dir().join(format!("nth-writable-{}", std::process::id()));
        std::fs::create_dir_all(root.join("source")).unwrap();
        std::os::unix::fs::symlink(std::env::temp_dir(), root.join("source/escape")).unwrap();
        let plan = Writable::Plan {
            file: root.join("plan.md"),
            memory: None,
            source: Some(root.join("source")),
        };
        assert!(plan.check(&root.join("source/new/a.html")).is_ok());
        assert!(plan.check(&root.join("other/source/a.html")).is_err());
        assert!(plan.check(&root.join("source/../state.json")).is_err());
        assert!(plan.check(&root.join("source/escape/a.html")).is_err());
        assert!(plan.check(&root.join("source/escape/../a.html")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
