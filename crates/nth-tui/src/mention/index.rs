//! The files a mention can complete to.

use std::path::Path;

use ignore::WalkBuilder;
use nth_session::CancellationToken;

/// Plenty for fuzzy matching by hand, and it stops a walk started from `~`
/// or `/` from reading the whole disk.
const MAX_FILES: usize = 20_000;

/// Files under `root` as `/`-separated relative paths. Skips hidden files
/// and whatever `.gitignore` and friends exclude, even outside a git repo.
/// Stops early, with what it has, once `cancel` fires.
pub fn walk(root: &Path, cancel: &CancellationToken) -> Vec<String> {
    WalkBuilder::new(root)
        .require_git(false)
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
        .take(MAX_FILES)
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

        assert_eq!(walk(root, &CancellationToken::new()), ["src/app/keys.rs"]);

        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(walk(root, &cancel).is_empty());
    }
}
