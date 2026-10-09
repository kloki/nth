//! The files a mention can complete to.

use std::path::{Path, PathBuf};

use ignore::WalkBuilder;
use nth_session::CancellationToken;

/// Plenty for fuzzy matching by hand, and it stops a walk started from `~`
/// or `/` from reading the whole disk.
const MAX_FILES: usize = 20_000;

/// The files under `cwd` as `/`-separated relative paths, then those under
/// each of `extra`, the directories added with `/add-dir`, by absolute
/// path: the tools take an absolute path as it is, wherever it points.
/// [`MAX_FILES`] is for them all together. Stops early, with what it has,
/// once `cancel` fires.
pub fn walk(cwd: &Path, extra: &[PathBuf], cancel: &CancellationToken) -> Vec<String> {
    let mut files = walk_one(cwd, MAX_FILES, cancel);
    for dir in extra {
        let room = MAX_FILES.saturating_sub(files.len());
        if room == 0 {
            break;
        }
        files.extend(
            walk_one(dir, room, cancel)
                .into_iter()
                .map(|relative| dir.join(relative).display().to_string()),
        );
    }
    files
}

/// At most `limit` files under `root` as `/`-separated relative paths.
/// Skips hidden files and whatever `.gitignore` and friends exclude. Like
/// git, a `.gitignore` only counts inside a repo, so a `*` in a dotfiles
/// `~/.gitignore` above it doesn't hide everything.
fn walk_one(root: &Path, limit: usize, cancel: &CancellationToken) -> Vec<String> {
    WalkBuilder::new(root)
        .build()
        .take_while(|_| !cancel.is_cancelled())
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|entry| {
            let relative = entry.path().strip_prefix(root).ok()?;
            let parts: Vec<_> = relative
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect();
            Some(parts.join("/"))
        })
        .take(limit)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn skips_ignored_and_hidden_files_and_stops_on_cancel() {
        let root = tempfile::tempdir().expect("tempdir");
        let root = root.path();
        for dir in ["src/app", "target", ".git"] {
            fs::create_dir_all(root.join(dir)).expect("mkdir");
        }
        for file in ["src/app/keys.rs", "target/out", ".git/config", ".env"] {
            fs::write(root.join(file), "").expect("write");
        }
        fs::write(root.join(".gitignore"), "target/\n").expect("write");

        assert_eq!(
            walk(root, &[], &CancellationToken::new()),
            ["src/app/keys.rs"]
        );

        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(walk(root, &[], &cancel).is_empty());
    }

    #[test]
    fn an_added_directory_lists_by_absolute_path_within_the_one_cap() {
        let cwd = tempfile::tempdir().expect("tempdir");
        let added = tempfile::tempdir().expect("tempdir");
        fs::write(cwd.path().join("here.rs"), "").expect("write");
        fs::write(added.path().join("there.rs"), "").expect("write");
        let extra = [added.path().to_path_buf()];

        assert_eq!(
            walk(cwd.path(), &extra, &CancellationToken::new()),
            [
                "here.rs".to_string(),
                format!("{}/there.rs", added.path().display())
            ]
        );
        // The working directory used up the cap, so nothing is added.
        assert!(walk_one(added.path(), 0, &CancellationToken::new()).is_empty());
    }
}
