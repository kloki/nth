//! How many lines the latest change to the plan added and removed, for the
//! tab's label.

use similar::{ChangeTag, TextDiff};

/// Lines added to and removed from `old` to make `new`, compared as the
/// plan tab shows them.
pub fn counts(old: &str, new: &str) -> (usize, usize) {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let diff = TextDiff::configure().diff_slices(&old, &new);
    let count = |tag| diff.iter_all_changes().filter(|c| c.tag() == tag).count();
    (count(ChangeTag::Insert), count(ChangeTag::Delete))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_added_and_removed_lines() {
        assert_eq!(
            counts("# Plan\nold step\nkeep\n", "# Plan\nnew step\nkeep\nmore\n"),
            (2, 1)
        );
    }

    #[test]
    fn a_new_plan_is_all_added() {
        assert_eq!(counts("", "a\nb"), (2, 0));
    }
}
