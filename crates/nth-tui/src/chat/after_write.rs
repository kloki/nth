//! What the write tool reports after the file is written: a note per
//! formatter that ran, and the errors language servers found, read back
//! from its result so they show under the call.

use std::path::Path;

use ratatui::{
    style::{Color, Style},
    text::Span,
};

use crate::theme::dim;

/// The most lines a write's report shows; the rest is summed up in one.
const MAX_NOTES: usize = 12;

#[derive(Debug, Clone, PartialEq)]
pub enum Note {
    /// A formatter's note, `Formatted with rustfmt.` or why it failed.
    Format(String),
    /// The file the errors below it are in.
    File(String),
    /// One error, `ERROR [2:18] mismatched types`.
    Error(String),
    /// More errors than are listed.
    More(String),
}

/// The notes in a write tool's result: everything after its first line,
/// without the headings and tags meant for the model.
pub fn parse(result: &str) -> Vec<Note> {
    let mut notes = Vec::new();
    for line in result.lines().skip(1) {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with("LSP errors detected") || line == "</diagnostics>" {
            continue;
        }
        let note = if let Some(file) = line
            .strip_prefix("<diagnostics file=\"")
            .and_then(|rest| rest.strip_suffix("\">"))
        {
            Note::File(file.to_string())
        } else if line.starts_with("ERROR ") {
            Note::Error(line.to_string())
        } else if line.starts_with("... and ") {
            Note::More(line.to_string())
        } else {
            Note::Format(line.to_string())
        };
        notes.push(note);
    }
    if notes.len() > MAX_NOTES {
        let cut = notes.len() - (MAX_NOTES - 1);
        notes.truncate(MAX_NOTES - 1);
        notes.push(Note::More(format!("... and {cut} more lines")));
    }
    notes
}

impl Note {
    /// The note as a styled span, paths relative to `cwd`.
    pub fn span(&self, cwd: &Path) -> Span<'static> {
        match self {
            Note::Format(text) | Note::More(text) => Span::styled(text.clone(), dim()),
            Note::File(file) => {
                let shown = Path::new(file)
                    .strip_prefix(cwd)
                    .map_or_else(|_| file.clone(), |rest| rest.display().to_string());
                Span::styled(shown, Style::new().fg(Color::Red))
            }
            Note::Error(text) => Span::styled(format!("  {text}"), Style::new().fg(Color::Red)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_format_notes_and_errors_per_file() {
        let result = "Wrote file: /repo/src/a.rs\n\n\
            Formatted with rustfmt.\n\n\
            LSP errors detected in this file, please fix:\n\
            <diagnostics file=\"/repo/src/a.rs\">\n\
            ERROR [2:18] mismatched types\n\
            </diagnostics>\n\n\
            LSP errors detected in other files:\n\
            <diagnostics file=\"/repo/src/b.rs\">\n\
            ERROR [1:1] unresolved import\n\
            ... and 3 more\n\
            </diagnostics>";

        assert_eq!(
            parse(result),
            [
                Note::Format("Formatted with rustfmt.".into()),
                Note::File("/repo/src/a.rs".into()),
                Note::Error("ERROR [2:18] mismatched types".into()),
                Note::File("/repo/src/b.rs".into()),
                Note::Error("ERROR [1:1] unresolved import".into()),
                Note::More("... and 3 more".into()),
            ]
        );
    }

    #[test]
    fn a_plain_write_has_no_notes() {
        assert!(parse("Wrote file: /repo/a.rs").is_empty());
    }

    #[test]
    fn a_long_report_is_cut() {
        let errors: Vec<String> = (1..=20).map(|i| format!("ERROR [{i}:1] bad")).collect();
        let result = format!(
            "Wrote file: /a\n\n<diagnostics file=\"/a\">\n{}\n</diagnostics>",
            errors.join("\n")
        );

        let notes = parse(&result);
        assert_eq!(notes.len(), MAX_NOTES);
        assert_eq!(
            notes[MAX_NOTES - 1],
            Note::More("... and 10 more lines".into())
        );
    }
}
