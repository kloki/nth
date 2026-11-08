//! The work plan on disk, and nothing that runs it: the items, cycles and
//! plan a session's orchestration writes under `.nth/work/<session>/`, the
//! markdown each item file holds, and the outline both front-ends draw.
//! nth-session's work tools write it; `nth work` and the TUI's work tab
//! read it.

mod format;
pub mod outline;
mod plan;
mod store;

use std::path::{Path, PathBuf};

pub use format::{one_line, parse, render, splits_sections, todo_counts, todo_lines};
pub use plan::{Cycle, Item, ItemRef, Plan, Status, Todo, WorkOutcome};
pub use store::Store;

/// Where the session `session` keeps its work plan: under `root`'s
/// `.nth/work`, `root` being the session's state root.
pub fn work_dir(root: &Path, session: &str) -> PathBuf {
    nth_directories::Directories::from_root(root.to_path_buf()).session_work(session)
}

/// A name for a cycle or an item: lowercase letters and digits, words
/// joined by single hyphens, at most 48 characters.
pub fn slug(text: &str) -> Result<(), String> {
    let fine = !text.is_empty()
        && text.len() <= 48
        && text.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        });
    match fine {
        true => Ok(()),
        false => Err(format!(
            "{text:?} is not a slug: use lowercase letters and digits, words joined by single hyphens, at most 48 characters"
        )),
    }
}

/// A cycle's folder name.
pub fn cycle_name(number: u32, slug: &str) -> String {
    format!("{number:02}-{slug}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_lowercase_words() {
        for fine in ["store", "a-b-2", "x", &"a".repeat(48)] {
            assert_eq!(slug(fine), Ok(()), "{fine}");
        }
        for bad in ["", "Foo", "a--b", "-a", "a-", "a b", "ä", &"a".repeat(49)] {
            assert!(slug(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn work_dir_is_under_the_main_checkout_of_a_linked_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let main = dir.path().join("main");
        let linked = main.join(".git/worktrees/feat");
        std::fs::create_dir_all(&linked).unwrap();
        let tree = dir.path().join("feat");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(tree.join(".git"), format!("gitdir: {}\n", linked.display())).unwrap();

        // The session's state root, as nth-session resolves it from the
        // worktree it runs in.
        let root = nth_directories::main_project_root(&tree).unwrap();

        assert_eq!(work_dir(&root, "s1"), main.join(".nth/work/s1"));
    }
}
