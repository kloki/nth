//! Applying a patch's hunks to a file's text, in memory: the tool writes
//! the result only once every section of the patch has applied.

use std::ops::Range;

use super::parse::Hunk;
use crate::{
    bom::BOM,
    edit::{
        matcher::{self, MatchError},
        with_line_endings,
    },
};

/// `source` with `hunks` applied in order, each found after the one before.
/// Like edit it keeps a BOM and speaks the file's line endings, and like
/// opencode it ends the file with a newline.
pub(super) fn apply(source: &str, hunks: &[Hunk]) -> Result<String, String> {
    let text = source.strip_prefix(BOM).unwrap_or(source);
    let crlf = text.contains("\r\n");
    let eol = if crlf { "\r\n" } else { "\n" };
    let text = if text.is_empty() || text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}{eol}")
    };

    let mut cursor = 0;
    let mut replacements = Vec::with_capacity(hunks.len());
    for (i, hunk) in hunks.iter().enumerate() {
        let n = i + 1;
        if let Some(context) = &hunk.context {
            let found = matcher::find_where(&text, context, |span| span.start >= cursor)
                .map_err(|_| {
                    if cursor == 0 {
                        format!("hunk {n}: the @@ line `{context}` is not in the file")
                    } else {
                        // It may well be in the file, before the cursor: hunks
                        // apply in order, so the model needs to merge or reorder.
                        format!(
                            "hunk {n}: the @@ line `{context}` is not in the file after hunk {}. Hunks apply in file order, each after the one before, so two changes under one @@ line go in one hunk, in order.",
                            n - 1
                        )
                    }
                })?;
            // Sorted, so the first is the nearest after the previous hunk.
            cursor = found
                .first()
                .map_or(cursor, |span| line_end(&text, span.end));
        }
        let new = joined(&hunk.new, eol);
        if hunk.old.is_empty() {
            // Pure additions go after the @@ line, or else at the end.
            let at = if hunk.context.is_some() {
                cursor
            } else {
                text.len()
            };
            replacements.push((at..at, new));
            continue;
        }
        let (span, new) = locate(&text, hunk, cursor, crlf).map_err(|e| {
            let expected = hunk.old.join("\n");
            match e {
                MatchError::NotFound => format!(
                    "hunk {n} not found. Its context and `-` lines must match the file, including indentation; read it again if unsure. Expected{}:\n{expected}",
                    if hunk.end_of_file { " at the end of the file" } else { "" }
                ),
                MatchError::Ambiguous(k) => format!(
                    "hunk {n} matches {k} places. Add an @@ line naming the enclosing function or class, or more context lines:\n{expected}"
                ),
            }
        })?;
        cursor = span.end;
        replacements.push((span, joined(new, eol)));
    }

    // Pure additions at the end may come before later hunks.
    replacements.sort_by_key(|(span, _)| span.start);
    let mut updated = String::with_capacity(source.len());
    if source.starts_with(BOM) {
        updated.push_str(BOM);
    }
    let mut from = 0;
    for (span, new) in &replacements {
        updated.push_str(&text[from..span.start]);
        updated.push_str(new);
        from = span.end;
    }
    updated.push_str(&text[from..]);
    Ok(updated)
}

/// The whole lines at or after `cursor` that hold the hunk's old lines,
/// and the new lines to put there. The old lines must match one place,
/// unless an @@ line anchors the hunk, which then takes the nearest.
fn locate<'h>(
    text: &str,
    hunk: &'h Hunk,
    cursor: usize,
    crlf: bool,
) -> Result<(Range<usize>, &'h [String]), MatchError> {
    // Only whole lines, give or take indentation, so `x = 1` never matches
    // the end of `max = 1`.
    let keep = |span: &Range<usize>| {
        span.start >= cursor
            && text[line_start(text, span.start)..span.start]
                .trim()
                .is_empty()
            && text[span.end..line_end(text, span.end)].trim().is_empty()
            && (!hunk.end_of_file || text[span.end..].trim().is_empty())
    };
    let (mut old, mut new) = (hunk.old.as_slice(), hunk.new.as_slice());
    let found = loop {
        let needle = with_line_endings(&joined(old, "\n"), crlf);
        match matcher::find_where(text, &needle, keep) {
            // A trailing empty line is often spacing before the next hunk
            // rather than a line the file has, so try without it.
            Err(MatchError::NotFound)
                if old.len() > 1 && old.last().is_some_and(String::is_empty) =>
            {
                old = &old[..old.len() - 1];
                if new.last().is_some_and(String::is_empty) {
                    new = &new[..new.len() - 1];
                }
            }
            result => break result?,
        }
    };
    let span = match found.as_slice() {
        [span] => span,
        [span, ..] if hunk.context.is_some() => span,
        many => return Err(MatchError::Ambiguous(many.len())),
    };
    Ok((line_start(text, span.start)..line_end(text, span.end), new))
}

fn joined(lines: &[String], eol: &str) -> String {
    lines.iter().map(|line| format!("{line}{eol}")).collect()
}

fn line_start(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// Past the newline ending the line `at` is on, or `at` itself if it is
/// already at the start of a line.
fn line_end(text: &str, at: usize) -> usize {
    if at == 0 || text[..at].ends_with('\n') {
        return at;
    }
    text[at..].find('\n').map_or(text.len(), |i| at + i + 1)
}
