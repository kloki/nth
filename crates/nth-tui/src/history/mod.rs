//! Prompt history: what was sent before, recalled with Up and Down and
//! kept across runs in one JSONL file, one prompt per line.

use std::{
    io,
    path::{Path, PathBuf},
};

use tokio::io::AsyncWriteExt;

/// Prompts kept; older ones are dropped on the next save.
pub const LIMIT: usize = 100;

/// Lines the file may hold before a save cuts it back to [`LIMIT`]. Saves
/// append so that instances running at once never drop each other's
/// prompts; only the cut rewrites the file, where a prompt another
/// instance appends in that moment can be lost, so it is kept rare.
const TRIM_PAST: usize = 2 * LIMIT;

#[derive(Debug, Default)]
pub struct History {
    /// Oldest first, at most [`LIMIT`].
    entries: Vec<String>,
    /// The entry being shown while recalling; `None` while typing afresh.
    at: Option<usize>,
    /// What was typed before the first Up, given back when Down passes the
    /// newest entry.
    draft: String,
    /// Where it is saved; `None` keeps it in memory only.
    path: Option<PathBuf>,
    /// Pushed since the last save took them, oldest first.
    unsaved: Vec<String>,
}

impl History {
    /// `$XDG_DATA_HOME/nth/prompt-history.jsonl`, next to the sessions.
    pub fn path() -> Result<PathBuf, nth_session::store::Error> {
        Ok(nth_session::store::data_dir()?.join("prompt-history.jsonl"))
    }

    /// Reads the history saved at `path`; a missing file is an empty
    /// history. One that can't be read is left alone rather than
    /// overwritten, and this run keeps its history in memory.
    pub async fn load(path: PathBuf) -> Self {
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => Self {
                path: Some(path),
                ..Self::parse(&text)
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self {
                path: Some(path),
                ..Self::default()
            },
            Err(_) => Self::default(),
        }
    }

    /// Skips lines that aren't a JSON string, so a damaged file loses only
    /// those; the next cut drops them from the file.
    fn parse(text: &str) -> Self {
        let mut entries: Vec<String> = text
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        entries.drain(..entries.len().saturating_sub(LIMIT));
        Self {
            entries,
            ..Self::default()
        }
    }

    /// The prompts pushed since the last call, as the lines a save appends.
    pub fn take_unsaved(&mut self) -> String {
        to_jsonl(&std::mem::take(&mut self.unsaved))
    }

    pub fn saved_at(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Stops saving, after a save failed, so the failure is told once.
    pub fn forget_path(&mut self) {
        self.path = None;
    }

    /// Records a sent prompt and stops recalling. Sending the newest entry
    /// again doesn't repeat it.
    pub fn push(&mut self, text: String) {
        self.at = None;
        self.draft.clear();
        if self.entries.last() != Some(&text) {
            self.unsaved.push(text.clone());
            self.unsaved
                .drain(..self.unsaved.len().saturating_sub(LIMIT));
            self.entries.push(text);
            self.entries
                .drain(..self.entries.len().saturating_sub(LIMIT));
        }
    }

    /// The entry before the one shown, with the prompt holding `current`.
    /// An edited entry is never thrown away: once the prompt differs from
    /// the entry shown, Up does nothing until the prompt is cleared.
    pub fn prev(&mut self, current: &str) -> Option<String> {
        let at = match self.at {
            Some(at) if self.entries[at] == current => at.checked_sub(1)?,
            Some(_) if !current.is_empty() => return None,
            _ => {
                self.draft = current.to_string();
                self.entries.len().checked_sub(1)?
            }
        };
        self.at = Some(at);
        Some(self.entries[at].clone())
    }

    /// The entry after the one shown, or the draft past the newest one.
    pub fn next(&mut self, current: &str) -> Option<String> {
        let at = self.at.filter(|&at| self.entries[at] == current)?;
        if at + 1 < self.entries.len() {
            self.at = Some(at + 1);
            Some(self.entries[at + 1].clone())
        } else {
            self.at = None;
            Some(std::mem::take(&mut self.draft))
        }
    }
}

fn to_jsonl(entries: &[String]) -> String {
    entries
        .iter()
        .map(|entry| serde_json::Value::from(entry.as_str()).to_string() + "\n")
        .collect()
}

/// Appends `jsonl` to the history at `path`, then cuts the file back to the
/// newest [`LIMIT`] prompts once it holds more than [`TRIM_PAST`] lines.
pub async fn append(path: &Path, jsonl: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    // One write, so another instance's line never lands inside these.
    file.write_all(jsonl.as_bytes()).await?;
    file.flush().await?;
    drop(file);

    let text = tokio::fs::read_to_string(path).await?;
    if text.lines().count() <= TRIM_PAST {
        return Ok(());
    }
    let kept = to_jsonl(&History::parse(&text).entries);
    // Written aside and renamed over, so a reader never sees half a file.
    let aside = path.with_extension(format!("jsonl.{}.tmp", std::process::id()));
    let written = match tokio::fs::write(&aside, kept).await {
        Ok(()) => tokio::fs::rename(&aside, path).await,
        Err(e) => Err(e),
    };
    if written.is_err() {
        let _ = tokio::fs::remove_file(&aside).await;
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(entries: &[&str]) -> History {
        let mut history = History::default();
        for entry in entries {
            history.push(entry.to_string());
        }
        history
    }

    #[test]
    fn parse_skips_damaged_lines_and_keeps_the_newest() {
        let mut text = String::from("\"one\"\nnot json\n42\n\"two\\nlines\"\n");
        let history = History::parse(&text);
        assert_eq!(history.entries, ["one", "two\nlines"]);
        assert_eq!(
            History::parse(&to_jsonl(&history.entries)).entries,
            history.entries
        );

        for i in 0..LIMIT {
            text.push_str(&format!("\"{i}\"\n"));
        }
        let history = History::parse(&text);
        assert_eq!(history.entries.len(), LIMIT);
        assert_eq!(history.entries[0], "0");
    }

    #[tokio::test]
    async fn appending_past_the_cut_keeps_the_newest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nth/prompt-history.jsonl");
        let lines: Vec<String> = (0..TRIM_PAST).map(|i| i.to_string()).collect();
        append(&path, &to_jsonl(&lines)).await.expect("appends");
        assert_eq!(History::load(path.clone()).await.entries.len(), LIMIT);

        append(&path, &to_jsonl(&["new".into()]))
            .await
            .expect("appends");

        let text = std::fs::read_to_string(&path).expect("reads");
        assert_eq!(text.lines().count(), LIMIT);
        assert_eq!(text.lines().last(), Some("\"new\""));
        assert_eq!(
            std::fs::read_dir(dir.path().join("nth"))
                .expect("lists")
                .count(),
            1
        );
    }

    #[test]
    fn push_skips_a_repeat_of_the_newest_and_trims() {
        let mut history = history(&["a", "b", "b", "a"]);
        assert_eq!(history.entries, ["a", "b", "a"]);

        for i in 0..LIMIT {
            history.push(i.to_string());
        }
        assert_eq!(history.entries.len(), LIMIT);
        assert_eq!(history.entries[0], "0");
    }

    #[test]
    fn up_and_down_walk_and_stop_at_both_ends() {
        let mut history = history(&["a", "b"]);
        assert_eq!(history.next(""), None);
        assert_eq!(history.prev("").as_deref(), Some("b"));
        assert_eq!(history.prev("b").as_deref(), Some("a"));
        assert_eq!(history.prev("a"), None);
        assert_eq!(history.next("a").as_deref(), Some("b"));
        assert_eq!(history.next("b").as_deref(), Some(""));
        assert_eq!(history.next(""), None);
    }

    #[test]
    fn down_past_the_newest_gives_the_draft_back() {
        let mut history = history(&["a"]);
        assert_eq!(history.prev("half typed").as_deref(), Some("a"));
        assert_eq!(history.next("a").as_deref(), Some("half typed"));
    }

    #[test]
    fn an_edited_entry_stops_the_walk_until_cleared() {
        let mut history = history(&["a", "b"]);
        history.prev("");
        assert_eq!(history.prev("b edited"), None);
        assert_eq!(history.next("b edited"), None);
        assert_eq!(history.prev("").as_deref(), Some("b"));
    }

    #[test]
    fn an_empty_history_recalls_nothing() {
        let mut history = History::default();
        assert_eq!(history.prev(""), None);
        assert_eq!(history.next(""), None);
    }
}
