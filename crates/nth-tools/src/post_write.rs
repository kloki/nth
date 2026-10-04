//! What happens after a tool writes a file: its formatters run, its
//! language servers check it, and the model is told what they found. Every
//! tool that writes files (write, and later edit and apply_patch) calls
//! [`PostWrite::after_write`].

use std::{path::Path, sync::Arc};

use nth_format::{Formatters, Outcome};
use nth_lsp::{Lsp, report};

use crate::bom;

#[derive(Clone)]
pub struct PostWrite {
    format: Arc<Formatters>,
    lsp: Lsp,
}

impl PostWrite {
    pub fn new(format: Arc<Formatters>, lsp: Lsp) -> Self {
        Self { format, lsp }
    }

    pub fn lsp(&self) -> &Lsp {
        &self.lsp
    }

    /// What to append to the tool's output after writing `path`: a note per
    /// formatter that ran, then the errors language servers report on the
    /// formatted file (and on up to five others), a blank line apart. Empty
    /// when nothing ran and nothing is wrong.
    pub async fn after_write(&self, path: &Path, cwd: &Path) -> String {
        let notes = self.format(path, cwd).await;
        // After formatting, so positions match the file as it now is.
        let diagnostics = self.lsp.touch(path, true).await;
        let errors = report::after_write(path, &diagnostics);
        join_sections(&notes, &errors)
    }

    /// Formats `path` with the formatters for `cwd`'s project and returns
    /// one note per formatter that ran. A BOM the file had before survives
    /// the formatter.
    async fn format(&self, path: &Path, cwd: &Path) -> String {
        let bom = bom::has_bom(path).await.unwrap_or(false);
        let outcomes = self.format.format(path, cwd).await;
        let mut notes: Vec<_> = outcomes.iter().map(note).collect();
        if !outcomes.is_empty()
            && let Err(e) = bom::sync(path, bom).await
        {
            notes.push(format!("Could not restore the byte order mark: {e}"));
        }
        notes.join("\n")
    }
}

/// The format notes, then the LSP report, a blank line apart. The report
/// comes with leading blank lines of its own, which go.
fn join_sections(notes: &str, errors: &str) -> String {
    let errors = errors.trim_start_matches('\n');
    match (notes.is_empty(), errors.is_empty()) {
        (_, true) => notes.to_string(),
        (true, false) => errors.to_string(),
        (false, false) => format!("{notes}\n\n{errors}"),
    }
}

fn note(outcome: &Outcome) -> String {
    match &outcome.result {
        Ok(()) => format!("Formatted with {}.", outcome.name),
        Err(e) => format!("{} failed: {e}", outcome.name),
    }
}

#[cfg(test)]
impl PostWrite {
    /// Formats nothing and checks nothing.
    pub(crate) fn off() -> Self {
        let config = nth_format::FormatConfig {
            enabled: false,
            ..Default::default()
        };
        Self::new(Arc::new(Formatters::new(&config)), lsp_off())
    }

    /// Runs `command` on `.txt` files and on nothing else, with no language
    /// servers.
    pub(crate) fn with_formatter(name: &str, command: &[&str]) -> Self {
        Self::new(formatter(name, command, ".txt"), lsp_off())
    }
}

/// Only the formatter `name`, on files with `extension`.
#[cfg(test)]
fn formatter(name: &str, command: &[&str], extension: &str) -> Arc<Formatters> {
    let mut config = nth_format::FormatConfig::default();
    let entry = nth_format::FormatterConfig {
        command: Some(command.iter().map(|s| s.to_string()).collect()),
        extensions: Some(vec![extension.into()]),
        ..Default::default()
    };
    config.formatters.insert(name.into(), entry);
    Arc::new(Formatters::new(&config))
}

/// Starts no language server.
#[cfg(test)]
pub(crate) fn lsp_off() -> Lsp {
    Lsp::new(&nth_lsp::LspConfig {
        enabled: false,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bom::BOM;

    #[tokio::test]
    async fn notes_each_formatter_that_ran() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a").expect("write");
        let post = PostWrite::with_formatter("sed", &["sed", "-i", "s/a/b/", "$FILE"]);

        let note = post.after_write(&path, dir.path()).await;

        assert_eq!(note, "Formatted with sed.");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "b");
    }

    #[tokio::test]
    async fn notes_a_failed_formatter_in_one_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a").expect("write");
        let post =
            PostWrite::with_formatter("bad", &["sh", "-c", "echo 'unexpected token' >&2; exit 1"]);

        let note = post.after_write(&path, dir.path()).await;

        assert_eq!(note, "bad failed: unexpected token");
    }

    #[tokio::test]
    async fn says_nothing_when_no_formatter_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.cs");
        std::fs::write(&path, "a").expect("write");
        let post = PostWrite::with_formatter("sed", &["sed", "-i", "s/a/b/", "$FILE"]);

        assert_eq!(post.after_write(&path, dir.path()).await, "");
    }

    #[tokio::test]
    async fn bom_survives_a_formatter_that_drops_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, format!("{BOM}a")).expect("write");
        let post = PostWrite::with_formatter("rewrite", &["sh", "-c", "printf b > $FILE"]);

        post.after_write(&path, dir.path()).await;

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            format!("{BOM}b")
        );
    }

    #[tokio::test]
    async fn formatter_does_not_add_a_bom_the_file_lacked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a").expect("write");
        let script = format!("printf '{BOM}b' > $FILE");
        let post = PostWrite::with_formatter("rewrite", &["sh", "-c", &script]);

        post.after_write(&path, dir.path()).await;

        assert_eq!(std::fs::read_to_string(&path).expect("read"), "b");
    }

    #[test]
    fn format_notes_come_before_lsp_errors() {
        let errors = "\n\nLSP errors detected in this file, please fix:\n<diagnostics>";
        assert_eq!(
            join_sections("Formatted with rustfmt.", errors),
            "Formatted with rustfmt.\n\nLSP errors detected in this file, please fix:\n<diagnostics>"
        );
        assert_eq!(
            join_sections("", errors),
            "LSP errors detected in this file, please fix:\n<diagnostics>"
        );
        assert_eq!(join_sections("Formatted with x.", ""), "Formatted with x.");
        assert_eq!(join_sections("", ""), "");
    }

    /// The whole path with a real rust-analyzer: run it by hand with
    /// `cargo test -p nth-tools -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs rust-analyzer on PATH"]
    async fn reports_rust_analyzer_errors_after_the_format_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        std::fs::create_dir_all(project.join("src")).expect("mkdir");
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"broken\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write");
        let path = project.join("src/main.rs");
        std::fs::write(&path, "fn main() {\n    let x: u32 = \"text\";\n}\n").expect("write");
        let post = PostWrite::new(
            formatter("noop", &["true"], ".rs"),
            Lsp::new(&nth_lsp::LspConfig::default()),
        );

        // rust-analyzer only checks once it has loaded the crate, which can
        // take longer than one write waits.
        let mut out = String::new();
        for _ in 0..30 {
            out = post.after_write(&path, project).await;
            if out.contains("LSP errors") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        println!("{out}");
        let note = out.find("Formatted with noop.").expect("format note");
        let errors = out
            .find("\n\nLSP errors detected in this file, please fix:\n")
            .expect("lsp errors");
        assert!(note < errors, "{out}");
        assert!(out.contains("ERROR [2:"), "{out}");
    }
}
