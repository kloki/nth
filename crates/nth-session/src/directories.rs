//! Validation and merging of additional working directories.

use std::{
    io,
    path::{Path, PathBuf},
};

/// Expand a typed path relative to the working directory, including `~`.
pub fn expand(cwd: &Path, home: Option<&Path>, arg: &str) -> PathBuf {
    let home = home.filter(|home| !home.as_os_str().is_empty());
    nth_context::expand_home(arg, home).unwrap_or_else(|| {
        let path = Path::new(arg);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        }
    })
}

/// Why a directory cannot be added.
#[derive(Debug)]
pub enum Refusal {
    /// The path could not be canonicalized.
    Inaccessible { path: PathBuf, error: io::Error },
    /// The path exists but is not a directory.
    NotDirectory(PathBuf),
    /// The canonical path overlaps an existing directory.
    Overlap {
        other: PathBuf,
        working_directory: bool,
        kind: Overlap,
    },
}

/// How a candidate relates to an existing directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overlap {
    Equal,
    Inside,
    Contains,
}

/// Canonicalize and validate a candidate against the cwd and saved directories.
pub fn validate(path: &Path, cwd: &Path, existing: &[PathBuf]) -> Result<PathBuf, Refusal> {
    let dir = std::fs::canonicalize(path).map_err(|error| Refusal::Inaccessible {
        path: path.to_path_buf(),
        error,
    })?;
    if !dir.is_dir() {
        return Err(Refusal::NotDirectory(path.to_path_buf()));
    }
    for (i, other) in std::iter::once(cwd)
        .chain(existing.iter().map(PathBuf::as_path))
        .enumerate()
    {
        let other = std::fs::canonicalize(other).unwrap_or_else(|_| other.to_path_buf());
        let kind = if dir == other {
            Some(Overlap::Equal)
        } else if dir.starts_with(&other) {
            Some(Overlap::Inside)
        } else if other.starts_with(&dir) {
            Some(Overlap::Contains)
        } else {
            None
        };
        if let Some(kind) = kind {
            return Err(Refusal::Overlap {
                other,
                working_directory: i == 0,
                kind,
            });
        }
    }
    Ok(dir)
}

/// The merged saved and configured directories, and invalid configured paths.
#[derive(Debug)]
pub struct Resolved {
    pub directories: Vec<PathBuf>,
    /// Each refusal identifies the configured path; overlaps are silently skipped.
    pub warnings: Vec<Refusal>,
}

/// Append configured absolute paths to saved directories, retaining saved entries.
/// Callers can label warnings with the originating config source and key.
pub fn resolve(cwd: &Path, saved: &[PathBuf], configured: &[PathBuf]) -> Resolved {
    let mut result = Resolved {
        directories: saved.to_vec(),
        warnings: Vec::new(),
    };
    for path in configured {
        match validate(path, cwd, &result.directories) {
            Ok(dir) => result.directories.push(dir),
            Err(Refusal::Overlap { .. }) => {}
            Err(warning) => result.warnings.push(warning),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_paths() {
        let cwd = Path::new("/repo");
        assert_eq!(
            expand(cwd, Some(Path::new("/home/me")), "~/docs"),
            PathBuf::from("/home/me/docs")
        );
        assert_eq!(expand(cwd, None, "../docs"), PathBuf::from("/repo/../docs"));
        assert_eq!(expand(cwd, None, "/docs"), PathBuf::from("/docs"));
    }

    #[test]
    fn merges_without_losing_saved_and_reports_invalid_paths() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let saved = root.path().join("saved");
        let added = root.path().join("added");
        for path in [&repo, &saved, &added, &added.join("child")] {
            std::fs::create_dir_all(path).unwrap();
        }
        let missing_saved = root.path().join("old");
        let file = root.path().join("file");
        std::fs::write(&file, "").unwrap();
        let missing = root.path().join("missing");
        let result = resolve(
            &repo,
            &[saved.clone(), missing_saved.clone()],
            &[
                repo.join(".."),
                saved.clone(),
                added.clone(),
                added.join("child"),
                file.clone(),
                missing.clone(),
            ],
        );
        assert_eq!(
            result.directories,
            [saved, missing_saved, added.canonicalize().unwrap()]
        );
        assert!(
            matches!(&result.warnings[..], [Refusal::NotDirectory(p), Refusal::Inaccessible { path, .. }] if p == &file && path == &missing)
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_both_overlap_directions() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let other = root.path().join("other");
        std::fs::create_dir_all(repo.join("child")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();
        assert!(matches!(
            validate(&link, &repo, &[]),
            Err(Refusal::Overlap {
                kind: Overlap::Equal,
                ..
            })
        ));
        assert!(matches!(
            validate(&link.join("child"), &repo, &[]),
            Err(Refusal::Overlap {
                kind: Overlap::Inside,
                ..
            })
        ));
        assert!(matches!(
            validate(root.path(), &repo, &[]),
            Err(Refusal::Overlap {
                kind: Overlap::Contains,
                ..
            })
        ));
    }
}
