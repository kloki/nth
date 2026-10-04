//! The text the model sees, word for word as opencode writes it
//! (`lsp/diagnostic.ts`, and `tool/write.ts`, `edit.ts` and
//! `apply_patch.ts`).

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use crate::types::Diagnostic;

const MAX_PER_FILE: usize = 20;
const MAX_OTHER_FILES: usize = 5;

/// A `<diagnostics>` block with the file's errors, or `""` when it has none.
/// Warnings and hints are left out: they are noise the model would chase.
pub fn report(file: &str, diagnostics: &[Diagnostic]) -> String {
    let errors: Vec<&Diagnostic> = diagnostics.iter().filter(|d| d.is_error()).collect();
    if errors.is_empty() {
        return String::new();
    }
    let lines: Vec<String> = errors
        .iter()
        .take(MAX_PER_FILE)
        .map(|d| pretty(d))
        .collect();
    let more = match errors.len().saturating_sub(MAX_PER_FILE) {
        0 => String::new(),
        more => format!("\n... and {more} more"),
    };
    format!(
        "<diagnostics file=\"{file}\">\n{}{more}\n</diagnostics>",
        lines.join("\n")
    )
}

fn pretty(diagnostic: &Diagnostic) -> String {
    let severity = match diagnostic.severity.unwrap_or(1) {
        2 => "WARN",
        3 => "INFO",
        4 => "HINT",
        _ => "ERROR",
    };
    let start = diagnostic.range.start;
    format!(
        "{severity} [{}:{}] {}",
        start.line + 1,
        start.character + 1,
        diagnostic.message
    )
}

/// What the write tool appends after writing `path`: this file's errors,
/// then errors in at most five other files, in path order.
pub fn after_write(path: &Path, diagnostics: &BTreeMap<PathBuf, Vec<Diagnostic>>) -> String {
    let mut out = String::new();
    let mut others = 0;
    for (file, issues) in diagnostics {
        let current = file == path;
        if !current && others >= MAX_OTHER_FILES {
            continue;
        }
        let block = report(&file.display().to_string(), issues);
        if block.is_empty() {
            continue;
        }
        if current {
            out.push_str(&after_edit(path, issues));
            continue;
        }
        others += 1;
        out.push_str(&format!("\n\nLSP errors detected in other files:\n{block}"));
    }
    out
}

/// What the edit tool appends after editing `path`: its errors and no
/// other file's.
pub fn after_edit(path: &Path, diagnostics: &[Diagnostic]) -> String {
    let block = report(&path.display().to_string(), diagnostics);
    if block.is_empty() {
        return block;
    }
    format!("\n\nLSP errors detected in this file, please fix:\n{block}")
}

/// What the apply_patch tool appends for each file it changed; `name` is
/// the file as the model knows it, relative to the working directory.
pub fn after_patch(name: &str, path: &Path, diagnostics: &[Diagnostic]) -> String {
    let block = report(&path.display().to_string(), diagnostics);
    if block.is_empty() {
        return block;
    }
    format!("\n\nLSP errors detected in {name}, please fix:\n{block}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Position, Range};

    fn diag(line: u32, character: u32, severity: Option<u8>, message: &str) -> Diagnostic {
        let at = Position { line, character };
        Diagnostic {
            range: Range { start: at, end: at },
            severity,
            code: None,
            source: None,
            message: message.into(),
        }
    }

    #[test]
    fn only_errors_with_one_based_positions() {
        let diags = [
            diag(0, 4, Some(1), "mismatched types"),
            diag(2, 0, Some(2), "unused variable"),
            diag(9, 9, None, "no severity"),
        ];
        assert_eq!(
            report("src/main.rs", &diags),
            "<diagnostics file=\"src/main.rs\">\nERROR [1:5] mismatched types\n</diagnostics>"
        );
    }

    #[test]
    fn nothing_without_errors() {
        assert_eq!(report("a.rs", &[]), "");
        assert_eq!(report("a.rs", &[diag(0, 0, Some(2), "warn")]), "");
    }

    #[test]
    fn caps_at_twenty_and_counts_the_rest() {
        let diags: Vec<_> = (0..23).map(|i| diag(i, 0, Some(1), "e")).collect();
        let text = report("a.rs", &diags);
        assert_eq!(text.lines().filter(|l| l.starts_with("ERROR")).count(), 20);
        assert!(text.ends_with("ERROR [20:1] e\n... and 3 more\n</diagnostics>"));
    }

    #[test]
    fn after_write_puts_this_file_and_five_others() {
        let mut all = BTreeMap::new();
        all.insert(
            PathBuf::from("/p/main.rs"),
            vec![diag(0, 0, Some(1), "here")],
        );
        for i in 0..7 {
            all.insert(
                PathBuf::from(format!("/p/other{i}.rs")),
                vec![diag(0, 0, Some(1), "there")],
            );
        }
        all.insert(PathBuf::from("/p/clean.rs"), vec![diag(0, 0, Some(2), "w")]);

        let text = after_write(Path::new("/p/main.rs"), &all);
        assert!(text.starts_with(
            "\n\nLSP errors detected in this file, please fix:\n<diagnostics file=\"/p/main.rs\">\nERROR [1:1] here\n</diagnostics>"
        ));
        assert_eq!(
            text.matches("LSP errors detected in other files:\n")
                .count(),
            5
        );
        assert!(text.contains("other4.rs") && !text.contains("other5.rs"));
        assert!(!text.contains("clean.rs"));
    }

    #[test]
    fn after_write_is_empty_when_all_is_well() {
        assert_eq!(after_write(Path::new("/p/a.rs"), &BTreeMap::new()), "");
    }

    #[test]
    fn after_edit_names_this_file() {
        let text = after_edit(Path::new("/p/a.rs"), &[diag(1, 2, Some(1), "bad")]);
        assert_eq!(
            text,
            "\n\nLSP errors detected in this file, please fix:\n<diagnostics file=\"/p/a.rs\">\nERROR [2:3] bad\n</diagnostics>"
        );
        assert_eq!(after_edit(Path::new("/p/a.rs"), &[]), "");
    }

    #[test]
    fn after_patch_names_the_file_by_its_relative_path() {
        let text = after_patch(
            "src/a.rs",
            Path::new("/p/src/a.rs"),
            &[diag(0, 0, Some(1), "bad")],
        );
        assert_eq!(
            text,
            "\n\nLSP errors detected in src/a.rs, please fix:\n<diagnostics file=\"/p/src/a.rs\">\nERROR [1:1] bad\n</diagnostics>"
        );
        assert_eq!(
            after_patch("a.rs", Path::new("/p/a.rs"), &[diag(0, 0, Some(2), "w")]),
            ""
        );
    }
}
