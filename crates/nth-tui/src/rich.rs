//! Markdown and code as styled lines, through hoodrich. The only place
//! that touches it: hoodrich gives one line per source line and borrows
//! the source, so what comes out of here is owned, and wrapped where asked.

use std::{ops::Range, path::Path};

use hoodrich::{Change, Mode, Renderer};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar;

/// Markdown as wrapped rows, with where the links in it ended up.
pub struct Markdown {
    pub lines: Vec<Line<'static>>,
    pub links: Vec<Link>,
}

/// A link's visible text on one wrapped row, and where it points. A link
/// that wraps is one of these per row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub row: usize,
    /// Display columns of the text on that row.
    pub columns: Range<usize>,
    pub url: String,
}

impl Link {
    /// The same link `by` columns to the right, as a prefix put in front
    /// of its row moves it.
    pub fn shifted(mut self, by: usize) -> Self {
        self.columns = self.columns.start + by..self.columns.end + by;
        self
    }
}

/// Markdown with its syntax hidden, wrapped to `width`.
pub fn markdown(text: &str, width: usize) -> Markdown {
    let text = text.replace('\t', "    ");
    let rendered = renderer(Mode::Concealed, width).render_with_links(&text);
    let mut lines = Vec::new();
    let mut links = Vec::new();
    for (source, line) in rendered.text.lines.into_iter().enumerate() {
        let first = lines.len();
        for (offset, row) in wrap_rows(line, width).into_iter().enumerate() {
            // hoodrich's columns are of the unwrapped line; the part of a
            // link this row shows is where they overlap.
            for link in rendered.links.iter().filter(|link| link.line == source) {
                let start = link.columns.start.max(row.columns.start);
                let end = link.columns.end.min(row.columns.end);
                if start < end {
                    links.push(Link {
                        row: first + offset,
                        columns: start - row.columns.start + row.hang
                            ..end - row.columns.start + row.hang,
                        url: link.url.clone(),
                    });
                }
            }
            lines.push(row.line);
        }
    }
    Markdown { lines, links }
}

/// A file's content, one line per line of `source`, highlighted by the
/// language `path` names. Code fills `width` with its background. Markdown
/// shows as written, its syntax dimmed, as a file is what it holds.
pub fn code(source: &str, path: &str, width: usize) -> Vec<Line<'static>> {
    let source = source.replace('\t', "    ");
    let lines = if is_markdown(path) {
        renderer(Mode::Raw, width).render(&source)
    } else {
        renderer(Mode::Concealed, width).render_code(&source, path)
    };
    lines.lines.into_iter().map(owned).collect()
}

/// `old` turned into `new` in the file at `path`, one line per line:
/// unchanged lines highlighted, added and removed ones plain.
pub fn code_diff(old: &str, new: &str, path: &str, width: usize) -> Vec<(Change, Line<'static>)> {
    let (old, new) = (old.replace('\t', "    "), new.replace('\t', "    "));
    let renderer = renderer(Mode::Raw, width);
    let lines = if is_markdown(path) {
        renderer.render_diff(&old, &new)
    } else {
        renderer.render_code_diff(&old, &new, path)
    };
    lines
        .into_iter()
        .map(|diff| (diff.change, owned(diff.line)))
        .collect()
}

/// Markdown diff of a document, for the plan tab: unchanged lines with
/// their syntax hidden, added and removed ones plain.
pub fn markdown_diff(old: &str, new: &str, width: usize) -> Vec<(Change, Line<'static>)> {
    let (old, new) = (old.replace('\t', "    "), new.replace('\t', "    "));
    renderer(Mode::Concealed, width)
        .render_diff(&old, &new)
        .into_iter()
        .map(|diff| (diff.change, owned(diff.line)))
        .collect()
}

fn renderer(mode: Mode, width: usize) -> Renderer {
    Renderer::new(mode).with_width(u16::try_from(width).unwrap_or(u16::MAX))
}

fn is_markdown(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|ext| ext == "md" || ext == "markdown")
}

pub fn owned(line: Line<'_>) -> Line<'static> {
    Line {
        spans: line
            .spans
            .into_iter()
            .map(|span| Span::styled(span.content.into_owned(), span.style))
            .collect(),
        style: line.style,
        alignment: line.alignment,
    }
}

/// Wraps a styled line at spaces to rows of at most `width` columns,
/// breaking words that are wider than a row. Rows after the first hang
/// under the line's indent and list marker. A line whose end has a
/// background, as code has, keeps it across every row. Every row carries
/// its style on its spans, none on the line.
pub fn wrap(line: Line<'_>, width: usize) -> Vec<Line<'static>> {
    wrap_rows(line, width)
        .into_iter()
        .map(|row| row.line)
        .collect()
}

/// One row of a wrapped line and where in the line it came from.
struct Row {
    line: Line<'static>,
    /// Display columns of the unwrapped line this row shows.
    columns: Range<usize>,
    /// Columns of indent put in front of the text.
    hang: usize,
}

fn wrap_rows(line: Line<'_>, width: usize) -> Vec<Row> {
    let width = width.max(1);
    if line.width() <= width {
        // The line's own style goes onto its spans, so a caller can put
        // them behind its own.
        let style = line.style;
        let columns = 0..line.width();
        let spans = owned(line).spans.into_iter();
        return vec![Row {
            line: Line::from(
                spans
                    .map(|span| {
                        let patched = style.patch(span.style);
                        span.style(patched)
                    })
                    .collect::<Vec<_>>(),
            ),
            columns,
            hang: 0,
        }];
    }
    let chars: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|span| {
            let style = line.style.patch(span.style);
            span.content.chars().map(move |c| (c, style))
        })
        .collect();
    let fill = chars
        .last()
        .and_then(|(_, style)| style.bg)
        .map(|bg| Style::new().bg(bg));
    let text: String = chars.iter().map(|(c, _)| c).collect();
    let hang = match hang(&text) {
        hang if hang * 2 > width => 0,
        hang => hang,
    };

    let mut rows: Vec<Row> = Vec::new();
    let mut start = 0;
    // Display column of `chars[start]` in the unwrapped line.
    let mut column = 0;
    while start < chars.len() {
        let room = if rows.is_empty() { width } else { width - hang };
        let mut end = start;
        let mut used = 0;
        while end < chars.len() {
            let w = chars[end].0.width().unwrap_or(0);
            if used + w > room && end > start {
                break;
            }
            used += w;
            end += 1;
        }
        let mut next = end;
        if end < chars.len() {
            // Break after the last space in the row, unless the word fills it.
            if let Some(space) = (start + 1..=end)
                .rev()
                .find(|&i| chars[i].0 == ' ' && chars[i - 1].0 != ' ')
            {
                end = space;
                next = space;
            }
            while next < chars.len() && chars[next].0 == ' ' {
                next += 1;
            }
        }
        let mut row = &chars[start..end];
        if fill.is_none() {
            while let Some(((' ', _), rest)) = row.split_last() {
                row = rest;
            }
        }
        let shown = width_of(row);
        let hang = if rows.is_empty() { 0 } else { hang };
        let mut spans = Vec::new();
        if hang > 0 {
            spans.push(Span::styled(" ".repeat(hang), fill.unwrap_or_default()));
        }
        spans.extend(spans_of(row));
        let mut out = Line::from(spans);
        if let Some(fill) = fill {
            let pad = width.saturating_sub(out.width());
            if pad > 0 {
                out.spans.push(Span::styled(" ".repeat(pad), fill));
            }
        }
        rows.push(Row {
            line: out,
            columns: column..column + shown,
            hang,
        });
        column += width_of(&chars[start..next]);
        start = next;
    }
    rows
}

fn width_of(chars: &[(char, Style)]) -> usize {
    chars.iter().map(|(c, _)| c.width().unwrap_or(0)).sum()
}

/// How far rows after the first sit in: the leading spaces, then a list
/// or task marker (`• `, `☐ `, `1. `) if one follows.
fn hang(text: &str) -> usize {
    let rest = text.trim_start_matches(' ');
    let indent = text.len() - rest.len();
    let marker = if ["• ", "☐ ", "☑ "].iter().any(|m| rest.starts_with(m)) {
        2
    } else {
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        let after = &rest[digits..];
        if digits > 0 && (after.starts_with(". ") || after.starts_with(") ")) {
            digits + 2
        } else {
            0
        }
    };
    indent + marker
}

/// Runs of chars that share a style, as spans.
fn spans_of(chars: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current = String::new();
    let mut style = None;
    for &(c, s) in chars {
        if style.is_some_and(|style| style != s) {
            spans.push(Span::styled(
                std::mem::take(&mut current),
                style.unwrap_or_default(),
            ));
        }
        style = Some(s);
        current.push(c);
    }
    if let Some(style) = style {
        spans.push(Span::styled(current, style));
    }
    spans
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::*;

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(Line::to_string).collect()
    }

    #[test]
    fn a_link_that_fits_keeps_its_columns() {
        let md = markdown("see [docs](https://x.y) now", 40);

        assert_eq!(text(&md.lines), ["see docs now"]);
        assert_eq!(
            md.links,
            [Link {
                row: 0,
                columns: 4..8,
                url: "https://x.y".into()
            }]
        );
    }

    #[test]
    fn a_link_on_a_wrapped_row_hangs_with_it() {
        let md = markdown("- alpha beta [gamma](u) delta", 14);

        assert_eq!(text(&md.lines), ["• alpha beta", "  gamma delta"]);
        assert_eq!(
            md.links,
            [Link {
                row: 1,
                columns: 2..7,
                url: "u".into()
            }]
        );
    }

    #[test]
    fn a_link_broken_by_wrapping_is_reported_on_both_rows() {
        let md = markdown("[one two](u)", 4);

        assert_eq!(text(&md.lines), ["one", "two"]);
        assert_eq!(
            md.links,
            [
                Link {
                    row: 0,
                    columns: 0..3,
                    url: "u".into()
                },
                Link {
                    row: 1,
                    columns: 0..3,
                    url: "u".into()
                }
            ]
        );
    }

    #[test]
    fn wrapping_keeps_each_fragments_style() {
        let lines = markdown("one **two** three four", 10).lines;

        assert_eq!(text(&lines), ["one two", "three four"]);
        let two = lines[0]
            .spans
            .iter()
            .find(|s| s.content == "two")
            .expect("two");
        assert!(two.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn rows_hang_under_a_bullet() {
        let lines = markdown("- alpha beta gamma", 12).lines;

        assert_eq!(text(&lines), ["• alpha beta", "  gamma"]);
    }

    #[test]
    fn a_word_wider_than_a_row_is_broken() {
        let lines = wrap(Line::raw("abcdefghij"), 4);

        assert_eq!(text(&lines), ["abcd", "efgh", "ij"]);
    }

    #[test]
    fn a_line_as_wide_as_the_row_stays_one_row() {
        assert_eq!(wrap(Line::raw("abcd efgh"), 9).len(), 1);
    }

    #[test]
    fn wrapped_code_keeps_its_background_on_every_row() {
        let lines = markdown("```\nlet value = something_long;\n```", 12).lines;
        let code: Vec<_> = lines
            .iter()
            .filter(|l| l.spans.iter().any(|s| s.content.contains("let")))
            .collect();
        assert_eq!(code.len(), 1);

        for line in &lines {
            if line.to_string().trim().is_empty() {
                continue;
            }
            assert_eq!(line.width(), 12, "{line}");
            let last = line.spans.last().expect("span");
            assert!(matches!(last.style.bg, Some(Color::Rgb(..))), "{line}");
        }
    }

    #[test]
    fn code_is_highlighted_by_its_path() {
        let lines = code("fn main() {}", "src/main.rs", 20);

        assert_eq!(lines.len(), 1);
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|s| s.content == "fn" && matches!(s.style.fg, Some(Color::Rgb(..))))
        );
    }
}
