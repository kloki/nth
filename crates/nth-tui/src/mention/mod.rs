//! File mentions: an `@` starting any word of the prompt, completed by fuzzy
//! matching against the files under the working directory.

mod index;

pub use index::walk;
use nucleo_matcher::{
    Config, Matcher,
    pattern::{AtomKind, CaseMatching, Normalization, Pattern},
};

/// Rows in the popup; typing narrows the list rather than scrolling it.
pub const LIMIT: usize = 8;

/// The mention being typed: `start` is the byte offset of its `@`.
#[derive(Debug, PartialEq)]
pub struct Mention<'a> {
    pub start: usize,
    pub query: &'a str,
}

/// The mention ending at `cursor`, if the word it sits in starts with `@`.
/// Requiring `@` at the start of a word keeps `me@example.com` out.
pub fn find(text: &str, cursor: usize) -> Option<Mention<'_>> {
    let before = &text[..cursor];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let query = before[start..].strip_prefix('@')?;
    Some(Mention { start, query })
}

/// The best `limit` files for `query`, shorter paths first among equals.
pub fn matches(files: &[String], query: &str, limit: usize) -> Vec<String> {
    let pattern = Pattern::new(
        query,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
    let mut scored = pattern.match_list(files, &mut matcher);
    scored.sort_by(|(a, x), (b, y)| {
        y.cmp(x)
            .then_with(|| a.len().cmp(&b.len()))
            .then_with(|| a.cmp(b))
    });
    scored
        .into_iter()
        .take(limit)
        .map(|(file, _)| file.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Option<Mention<'_>> {
        find(text, text.len())
    }

    #[test]
    fn finds_a_mention_at_the_start_of_any_word() {
        assert_eq!(
            at("@ke"),
            Some(Mention {
                start: 0,
                query: "ke"
            })
        );
        assert_eq!(
            at("see @src/ke"),
            Some(Mention {
                start: 4,
                query: "src/ke"
            })
        );
        assert_eq!(
            at("one\n@"),
            Some(Mention {
                start: 4,
                query: ""
            })
        );
        assert_eq!(at("mail me@example.com"), None);
        assert_eq!(at("@ke done"), None);
    }

    #[test]
    fn only_looks_behind_the_cursor() {
        assert_eq!(
            find("see @keys and more", 7),
            Some(Mention {
                start: 4,
                query: "ke"
            })
        );
        assert_eq!(find("see @keys and more", 12), None);
    }

    #[test]
    fn ranks_fuzzy_matches() {
        let files: Vec<String> = [
            "crates/nth-tui/src/app/keys.rs",
            "crates/nth-tui/src/app/mod.rs",
            "README.md",
            "Cargo.toml",
        ]
        .map(String::from)
        .into();

        assert_eq!(
            matches(&files, "keys", LIMIT),
            ["crates/nth-tui/src/app/keys.rs"]
        );
        assert_eq!(matches(&files, "", 2), ["README.md", "Cargo.toml"]);
        assert!(matches(&files, "zzz", LIMIT).is_empty());
    }
}
