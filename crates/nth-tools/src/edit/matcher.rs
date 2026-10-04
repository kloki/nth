//! Finds where a model's `oldString` sits in a file. Models copy text from
//! read output and often get the whitespace slightly wrong, so after an exact
//! match fails a few looser strategies try again. Ported from opencode's
//! `tool/edit.ts`, minus the block-anchor and Levenshtein ones, which can
//! match text the model never saw.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MatchError {
    NotFound,
    /// The needle matched this many places.
    Ambiguous(usize),
}

/// Each strategy returns every span of `content` it considers a match.
type Strategy = fn(&str, &str) -> Vec<Range<usize>>;

/// Strictest first, and the first that finds anything decides: a looser
/// strategy never picks one of several places a stricter one found.
/// Indentation-flexible is stricter than line-trimmed, as it still compares
/// each line's indent relative to the block's.
const STRATEGIES: [Strategy; 4] = [
    exact,
    indentation_flexible,
    line_trimmed,
    whitespace_normalised,
];

/// The one span of `content` that `needle` matches. An empty needle matches
/// nothing.
pub(crate) fn find_unique(content: &str, needle: &str) -> Result<Range<usize>, MatchError> {
    match first_found(content, needle)?.as_slice() {
        [span] => Ok(span.clone()),
        many => Err(MatchError::Ambiguous(many.len())),
    }
}

/// Every span `needle` matches, sorted and not overlapping. Never
/// `Ambiguous`.
pub(crate) fn find_all(content: &str, needle: &str) -> Result<Vec<Range<usize>>, MatchError> {
    let mut kept: Vec<Range<usize>> = Vec::new();
    for span in first_found(content, needle)? {
        if kept.last().is_none_or(|last| last.end <= span.start) {
            kept.push(span);
        }
    }
    Ok(kept)
}

/// The spans of the first strategy that finds any, sorted.
fn first_found(content: &str, needle: &str) -> Result<Vec<Range<usize>>, MatchError> {
    if needle.is_empty() {
        return Err(MatchError::NotFound);
    }
    STRATEGIES
        .iter()
        .map(|strategy| {
            let mut found = strategy(content, needle);
            found.sort_by_key(|span| (span.start, span.end));
            found.dedup();
            found
        })
        .find(|found| !found.is_empty())
        .ok_or(MatchError::NotFound)
}

/// Overlapping occurrences count, so `aa` in `aaa` is ambiguous.
fn exact(content: &str, needle: &str) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = content[from..].find(needle) {
        let start = from + at;
        found.push(start..start + needle.len());
        // Step one char, not one byte, to stay on a char boundary.
        from = start + content[start..].chars().next().map_or(1, char::len_utf8);
    }
    found
}

/// Lines equal once each is trimmed.
fn line_trimmed(content: &str, needle: &str) -> Vec<Range<usize>> {
    windows(content, needle, |line, want| line.trim() == want.trim())
}

/// Lines equal once the block's common indentation is removed, so a block
/// copied at the wrong depth still matches but its inner shape must agree.
fn indentation_flexible(content: &str, needle: &str) -> Vec<Range<usize>> {
    let want = Needle::new(needle);
    if want.is_blank() {
        return Vec::new();
    }
    let want_lines = dedent(&want.lines);
    let lines = Lines::new(content);
    (0..lines.windows(want.lines.len()))
        .filter(|&i| dedent(lines.text(i, want.lines.len())) == want_lines)
        .map(|i| lines.span(i, &want))
        .collect()
}

/// Every run of whitespace counts as one space, within a line or across a
/// block of lines.
fn whitespace_normalised(content: &str, needle: &str) -> Vec<Range<usize>> {
    let want = normalise(needle);
    if want.is_empty() {
        return Vec::new();
    }
    let lines = Lines::new(content);
    let mut found = Vec::new();
    for i in 0..lines.len() {
        let span = lines.span_of(i);
        let line = &content[span.clone()];
        if normalise(line) == want {
            found.push(span);
        } else if let Some(within) = words_in(line, &want) {
            found.push(span.start + within.start..span.start + within.end);
        }
    }
    let needle = Needle::new(needle);
    if needle.lines.len() > 1 {
        let n = needle.lines.len();
        for i in 0..lines.windows(n) {
            if normalise(&lines.text(i, n).join("\n")) == want {
                found.push(lines.span(i, &needle));
            }
        }
    }
    found
}

/// Runs of lines in `content` whose lines each pass `eq` against the
/// needle's lines in order.
fn windows(content: &str, needle: &str, eq: fn(&str, &str) -> bool) -> Vec<Range<usize>> {
    let want = Needle::new(needle);
    if want.is_blank() {
        return Vec::new();
    }
    let lines = Lines::new(content);
    let n = want.lines.len();
    (0..lines.windows(n))
        .filter(|&i| {
            lines
                .text(i, n)
                .iter()
                .zip(&want.lines)
                .all(|(line, want)| eq(line, want))
        })
        .map(|i| lines.span(i, &want))
        .collect()
}

/// A needle split into lines. A trailing newline is set apart rather than
/// kept as an empty last line, so `"foo\n"` matches the line `foo` and the
/// newline after it.
struct Needle<'a> {
    lines: Vec<&'a str>,
    trailing_newline: bool,
}

impl<'a> Needle<'a> {
    fn new(needle: &'a str) -> Self {
        let trimmed = needle.strip_suffix('\n');
        Self {
            lines: trimmed.unwrap_or(needle).split('\n').collect(),
            trailing_newline: trimmed.is_some(),
        }
    }

    /// A blank needle would match every blank line once whitespace stops
    /// counting, which is never what the model meant.
    fn is_blank(&self) -> bool {
        self.lines.iter().all(|line| line.trim().is_empty())
    }
}

/// `content` split on `\n`, keeping where each line starts and ends.
struct Lines<'a> {
    content: &'a str,
    lines: Vec<&'a str>,
    spans: Vec<Range<usize>>,
}

impl<'a> Lines<'a> {
    fn new(content: &'a str) -> Self {
        let lines: Vec<&str> = content.split('\n').collect();
        let mut start = 0;
        let spans = lines
            .iter()
            .map(|line| {
                let span = start..start + line.len();
                start = span.end + 1;
                span
            })
            .collect();
        Self {
            content,
            lines,
            spans,
        }
    }

    fn len(&self) -> usize {
        self.lines.len()
    }

    /// How many windows of `n` lines there are.
    fn windows(&self, n: usize) -> usize {
        (self.len() + 1).saturating_sub(n)
    }

    fn text(&self, i: usize, n: usize) -> &[&'a str] {
        &self.lines[i..i + n]
    }

    fn span_of(&self, i: usize) -> Range<usize> {
        self.spans[i].clone()
    }

    /// The bytes of the lines matched by `needle` from line `i`, plus the
    /// newline after them when the needle ends in one and the file has it.
    fn span(&self, i: usize, needle: &Needle) -> Range<usize> {
        let start = self.spans[i].start;
        let mut end = self.spans[i + needle.lines.len() - 1].end;
        if needle.trailing_newline && end < self.content.len() {
            end += 1;
        }
        start..end
    }
}

/// Strips the indentation all non-blank lines share; blank lines become
/// empty so trailing spaces on them don't matter.
fn dedent(lines: &[&str]) -> Vec<String> {
    let indent = |line: &str| line.len() - line.trim_start().len();
    let common = lines
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| indent(line))
        .min()
        .unwrap_or(0);
    lines
        .iter()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                // `get` because indentation may hold multi-byte whitespace.
                line.get(common..).unwrap_or(line.trim_start()).to_string()
            }
        })
        .collect()
}

fn normalise(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Where in `line` the words of `want` appear in order, separated by any
/// whitespace and nothing else.
fn words_in(line: &str, want: &str) -> Option<Range<usize>> {
    let words: Vec<&str> = want.split(' ').collect();
    let first = words.first()?;
    line.match_indices(first).find_map(|(start, _)| {
        let mut end = start + first.len();
        for word in &words[1..] {
            let rest = &line[end..];
            let gap = rest.len() - rest.trim_start().len();
            if gap == 0 || !rest[gap..].starts_with(word) {
                return None;
            }
            end += gap + word.len();
        }
        Some(start..end)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique<'a>(content: &'a str, needle: &str) -> &'a str {
        let span = find_unique(content, needle).expect("one match");
        &content[span]
    }

    #[test]
    fn exact_match() {
        assert_eq!(unique("let a = 1;\nlet b = 2;\n", "b = 2"), "b = 2");
    }

    #[test]
    fn exact_duplicates_are_ambiguous() {
        assert_eq!(find_unique("x\nx\nx\n", "x"), Err(MatchError::Ambiguous(3)));
        // Overlapping occurrences count too.
        assert_eq!(find_unique("aaa", "aa"), Err(MatchError::Ambiguous(2)));
    }

    #[test]
    fn missing_and_empty_needles_are_not_found() {
        assert_eq!(find_unique("abc", "xyz"), Err(MatchError::NotFound));
        assert_eq!(find_unique("abc", ""), Err(MatchError::NotFound));
        assert_eq!(find_all("abc", ""), Err(MatchError::NotFound));
    }

    #[test]
    fn line_trimmed_ignores_leading_and_trailing_space() {
        let content = "fn main() {\n    let x = 1;  \n    let y = 2;\n}\n";
        assert_eq!(
            unique(content, "let x = 1;\nlet y = 2;"),
            "    let x = 1;  \n    let y = 2;"
        );
    }

    #[test]
    fn fuzzy_matches_keep_a_trailing_newline() {
        let content = "a\n\tb\nc\n";
        assert_eq!(unique(content, "  b\n"), "\tb\n");
    }

    #[test]
    fn fuzzy_trailing_newline_at_end_of_file_without_one() {
        let content = "a\n  b";
        assert_eq!(unique(content, "b\n"), "  b");
    }

    #[test]
    fn indentation_flexible_keeps_the_block_shape() {
        // Both blocks trim to the same lines; only the first has the shape
        // of the needle, so line-trimmed alone would be ambiguous.
        let content = "a\n  b\n\n  a\n  b\n";
        let needle = "    a\n      b";
        assert_eq!(line_trimmed(content, needle).len(), 2);
        assert_eq!(unique(content, needle), "a\n  b");
    }

    #[test]
    fn indentation_flexible_ignores_spaces_on_blank_lines() {
        let content = "impl A {\n    fn f() {\n        1\n    \n    }\n}\n";
        let needle = "fn f() {\n    1\n\n}";
        assert_eq!(indentation_flexible(content, needle), vec![9..42]);
    }

    #[test]
    fn indentation_flexible_requires_the_same_shape() {
        let content = "    a\n    b\n";
        assert!(indentation_flexible(content, "a\n  b").is_empty());
        assert_eq!(indentation_flexible(content, "  a\n  b").len(), 1);
    }

    #[test]
    fn whitespace_normalised_within_a_line() {
        let content = "let  x   =\t1;\nother\n";
        assert_eq!(unique(content, "x = 1"), "x   =\t1");
        assert_eq!(unique(content, "let x = 1;"), "let  x   =\t1;");
    }

    #[test]
    fn whitespace_normalised_multi_line_needle() {
        let content = "if x  {\n\tgo( )\n}\n";
        assert_eq!(unique(content, "if x {\n  go( )\n}"), "if x  {\n\tgo( )\n}");
    }

    #[test]
    fn whitespace_normalised_needs_whitespace_between_words() {
        assert_eq!(words_in("foobar", "foo bar"), None);
        assert_eq!(words_in("a foo  bar", "foo bar"), Some(2..10));
    }

    #[test]
    fn fuzzy_duplicates_are_ambiguous() {
        let content = "  x = 1\n\n    x = 1\n";
        assert_eq!(
            find_unique(content, "x = 1 "),
            Err(MatchError::Ambiguous(2))
        );
    }

    #[test]
    fn looser_strategies_never_settle_an_ambiguity() {
        // Line-trimmed would find `b` alone on a line only twice, but exact
        // found three and looser strategies never get to pick.
        assert_eq!(
            find_unique("ab\nb\nb\n", "b"),
            Err(MatchError::Ambiguous(3))
        );
    }

    #[test]
    fn blank_needles_never_match_fuzzily() {
        assert_eq!(find_unique("a\n\nb\n", "  \n"), Err(MatchError::NotFound));
    }

    #[test]
    fn find_all_returns_every_exact_match_without_overlap() {
        assert_eq!(find_all("aaaa", "aa"), Ok(vec![0..2, 2..4]));
        assert_eq!(find_all("x y x", "x"), Ok(vec![0..1, 4..5]));
    }

    #[test]
    fn find_all_falls_back_to_fuzzy_strategies() {
        let content = "\tx = 1\n\ty\n\t\tx = 1\n";
        assert_eq!(find_all(content, "  x = 1\n"), Ok(vec![0..7, 10..18]));
    }

    #[test]
    fn multibyte_text_stays_on_char_boundaries() {
        assert_eq!(unique("héllo wörld", "wörld"), "wörld");
        assert_eq!(find_unique("ééé", "éé"), Err(MatchError::Ambiguous(2)));
        assert_eq!(unique("  ünï  code\n", "ünï code"), "  ünï  code");
        // The common indent here is one byte, inside the ideographic space.
        assert_eq!(dedent(&["\u{3000}a", " b"]), ["a", "b"]);
    }
}
