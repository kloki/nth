//! The pickers' filter, as in telescope: typing narrows the list to what
//! fuzzily matches, best first, and marks the characters that matched.
//! Lists are short, so every keystroke matches the whole list again.

use nucleo_matcher::{
    Config, Matcher, Utf32Str,
    pattern::{AtomKind, CaseMatching, Normalization, Pattern},
};
use ratatui::{
    Frame,
    layout::{Alignment, Position, Rect},
    style::{Color, Style},
    text::Span,
    widgets::Paragraph,
};

use crate::theme::{BAR_WIDTH, dim, panel_row};

/// What goes before the query on its row.
const PROMPT: &str = "> ";

#[derive(Debug, Default)]
pub struct Filter {
    query: String,
}

impl Filter {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn is_empty(&self) -> bool {
        self.query.is_empty()
    }

    pub fn push(&mut self, c: char) {
        self.query.push(c);
    }

    pub fn pop(&mut self) {
        self.query.pop();
    }

    pub fn clear(&mut self) {
        self.query.clear();
    }

    /// The indices of the `haystacks` that match, best first; equals keep
    /// their order, so an empty query keeps the list as it is.
    pub fn rank<'a>(&self, haystacks: impl IntoIterator<Item = &'a str>) -> Vec<usize> {
        let pattern = self.pattern();
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();
        let mut scored: Vec<(usize, u32)> = haystacks
            .into_iter()
            .enumerate()
            .filter_map(|(i, haystack)| {
                let score = pattern.score(Utf32Str::new(haystack, &mut buf), &mut matcher)?;
                Some((i, score))
            })
            .collect();
        scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
        scored.into_iter().map(|(i, _)| i).collect()
    }

    /// Which characters of `haystack` the query matched, in order.
    pub fn indices(&self, haystack: &str) -> Vec<u32> {
        let mut indices = Vec::new();
        if self.is_empty() {
            return indices;
        }
        let mut matcher = Matcher::new(Config::DEFAULT);
        let mut buf = Vec::new();
        self.pattern().indices(
            Utf32Str::new(haystack, &mut buf),
            &mut matcher,
            &mut indices,
        );
        indices.sort_unstable();
        indices.dedup();
        indices
    }

    fn pattern(&self) -> Pattern {
        Pattern::new(
            &self.query,
            CaseMatching::Smart,
            Normalization::Smart,
            AtomKind::Fuzzy,
        )
    }
}

/// `text` as spans in `base`, the characters `indices` marks in `hit`.
/// `text` starts at character `offset` of the haystack `indices` are into,
/// so one match can mark several columns of a row.
pub fn highlight<'a>(
    text: &str,
    indices: &[u32],
    offset: usize,
    base: Style,
    hit: Style,
) -> Vec<Span<'a>> {
    let mut spans: Vec<Span<'a>> = Vec::new();
    let mut run = String::new();
    let mut run_hit = false;
    for (i, c) in text.chars().enumerate() {
        let is_hit = u32::try_from(offset + i).is_ok_and(|at| indices.binary_search(&at).is_ok());
        if is_hit != run_hit && !run.is_empty() {
            let style = if run_hit { hit } else { base };
            spans.push(Span::styled(std::mem::take(&mut run), style));
        }
        run_hit = is_hit;
        run.push(c);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, if run_hit { hit } else { base }));
    }
    spans
}

/// A picker's query row: the bar, `> ` and the query with the cursor
/// after it, and how many of the `total` items it matches on the right.
pub fn query_row(
    frame: &mut Frame,
    area: Rect,
    accent: Color,
    query: &str,
    matched: usize,
    total: usize,
) {
    frame.render_widget(
        Paragraph::new(panel_row(
            accent,
            [Span::styled(PROMPT, dim()), Span::raw(query.to_string())],
        )),
        area,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(format!("{matched}/{total}"), dim()))
            .alignment(Alignment::Right),
        area,
    );
    let col = BAR_WIDTH as usize + PROMPT.len() + query.chars().count();
    let x = area.x + u16::try_from(col).unwrap_or(u16::MAX);
    frame.set_cursor_position(Position::new(x.min(area.right().saturating_sub(1)), area.y));
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::*;

    fn filter(query: &str) -> Filter {
        Filter {
            query: query.into(),
        }
    }

    #[test]
    fn an_empty_query_keeps_everything_in_order() {
        assert_eq!(filter("").rank(["b", "a", "c"]), [0, 1, 2]);
    }

    #[test]
    fn ranks_the_best_match_first_and_drops_the_rest() {
        let items = ["kimi-k2", "glm-4.6", "qwen3-coder", "glm-4.5-air"];
        let ranked = filter("glm").rank(items);
        assert_eq!(ranked, [1, 3], "equals keep their order");
        assert_eq!(filter("coder").rank(items), [2]);
        assert!(filter("zzz").rank(items).is_empty());
    }

    #[test]
    fn words_match_anywhere_and_case_is_smart() {
        let items = ["Fix the parser", "resume picker"];
        assert_eq!(filter("pick res").rank(items), [1]);
        assert_eq!(filter("fix").rank(items), [0]);
        assert!(filter("FIX").rank(items).is_empty());
    }

    #[test]
    fn marks_the_matched_characters() {
        let f = filter("gm");
        let indices = f.indices("glm x");
        assert_eq!(indices, [0, 2]);

        let hit = Style::new().add_modifier(Modifier::BOLD);
        let base = Style::new().fg(Color::Blue);
        let spans = highlight("glm", &indices, 0, base, hit);
        let parts: Vec<_> = spans
            .iter()
            .map(|s| (s.content.as_ref(), s.style == hit))
            .collect();
        assert_eq!(parts, [("g", true), ("l", false), ("m", true)]);

        let later = highlight("x", &f.indices("x glm"), 0, base, hit);
        assert_eq!(later.len(), 1);
        assert_eq!(highlight("lm", &indices, 1, base, hit)[1].content, "m");
    }
}
