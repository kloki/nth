//! What happens after a tool writes a file: its formatters run, and the
//! model is told what they did. Every tool that writes files (write, and
//! later edit and apply_patch) calls [`PostWrite::after_write`].

use std::{path::Path, sync::Arc};

use nth_format::{Formatters, Outcome};

use crate::bom;

#[derive(Clone)]
pub struct PostWrite {
    format: Arc<Formatters>,
}

impl PostWrite {
    pub fn new(format: Arc<Formatters>) -> Self {
        Self { format }
    }

    /// Formats `path` with the formatters for `cwd`'s project and returns
    /// one note per formatter that ran, to append to the tool's output.
    /// Empty when nothing ran. A BOM the file had before survives the
    /// formatter.
    pub async fn after_write(&self, path: &Path, cwd: &Path) -> String {
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

fn note(outcome: &Outcome) -> String {
    match &outcome.result {
        Ok(()) => format!("Formatted with {}.", outcome.name),
        Err(e) => format!("{} failed: {e}", outcome.name),
    }
}

#[cfg(test)]
impl PostWrite {
    /// Formats nothing.
    pub(crate) fn off() -> Self {
        let config = nth_format::FormatConfig {
            enabled: false,
            ..Default::default()
        };
        Self::new(Arc::new(Formatters::new(&config)))
    }

    /// Runs `command` on `.txt` files and on nothing else.
    pub(crate) fn with_formatter(name: &str, command: &[&str]) -> Self {
        let mut config = nth_format::FormatConfig::default();
        let entry = nth_format::FormatterConfig {
            command: Some(command.iter().map(|s| s.to_string()).collect()),
            extensions: Some(vec![".txt".into()]),
            ..Default::default()
        };
        config.formatters.insert(name.into(), entry);
        Self::new(Arc::new(Formatters::new(&config)))
    }
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
}
