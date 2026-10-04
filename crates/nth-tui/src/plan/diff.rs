//! A line diff of two versions of the plan, for the plan tab to colour.

use similar::{ChangeTag, TextDiff};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Same,
    Added,
    Removed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: Kind,
    /// Without its line ending.
    pub text: String,
}

/// Every line of `new`, with the lines of `old` it no longer has in their
/// place.
pub fn lines(old: &str, new: &str) -> Vec<Line> {
    TextDiff::from_lines(old, new)
        .iter_all_changes()
        .map(|change| Line {
            kind: match change.tag() {
                ChangeTag::Equal => Kind::Same,
                ChangeTag::Insert => Kind::Added,
                ChangeTag::Delete => Kind::Removed,
            },
            text: change.value().trim_end_matches(['\n', '\r']).to_string(),
        })
        .collect()
}

/// How many lines were added and removed.
pub fn counts(lines: &[Line]) -> (usize, usize) {
    let count = |kind| lines.iter().filter(|l| l.kind == kind).count();
    (count(Kind::Added), count(Kind::Removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_added_and_removed_lines() {
        let lines = lines("# Plan\nold step\nkeep\n", "# Plan\nnew step\nkeep\nmore\n");
        let kinds: Vec<(Kind, &str)> = lines.iter().map(|l| (l.kind, l.text.as_str())).collect();

        assert_eq!(
            kinds,
            [
                (Kind::Same, "# Plan"),
                (Kind::Removed, "old step"),
                (Kind::Added, "new step"),
                (Kind::Same, "keep"),
                (Kind::Added, "more"),
            ]
        );
        assert_eq!(counts(&lines), (2, 1));
    }

    #[test]
    fn a_new_plan_is_all_added() {
        assert_eq!(counts(&lines("", "a\nb")), (2, 0));
    }
}
