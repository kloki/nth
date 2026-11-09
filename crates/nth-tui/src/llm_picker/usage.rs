//! How many turns ran on each model, kept across runs in one JSON file:
//! the order of the picker, and the usage tab's `all time` graph.

use std::{
    cmp::Reverse,
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

pub type Turns = BTreeMap<String, u64>;

#[derive(Debug, Default)]
pub struct LlmUsage {
    /// What was saved at load, plus this run's turns.
    turns: Turns,
    /// This run's turns since the last save took them.
    unsaved: Turns,
    /// Where it is saved; `None` keeps it in memory only.
    path: Option<PathBuf>,
}

impl LlmUsage {
    /// `$XDG_DATA_HOME/nth/llm-usage.json`, next to the sessions.
    pub fn path() -> Result<PathBuf, nth_session::store::Error> {
        Ok(nth_session::store::data_dir()?.join("llm-usage.json"))
    }

    /// Reads the counts saved at `path`; a missing file is no usage yet.
    /// One that can't be read or parsed is left alone rather than
    /// overwritten, and this run counts in memory.
    pub async fn load(path: PathBuf) -> Self {
        match tokio::fs::read_to_string(&path).await {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(turns) => Self {
                    turns,
                    path: Some(path),
                    ..Self::default()
                },
                Err(_) => Self::default(),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self {
                path: Some(path),
                ..Self::default()
            },
            Err(_) => Self::default(),
        }
    }

    /// The turns counted since the last call, for a save to add.
    pub fn take_unsaved(&mut self) -> Turns {
        std::mem::take(&mut self.unsaved)
    }

    pub fn saved_at(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Stops saving, after a save failed, so the failure is told once.
    pub fn forget_path(&mut self) {
        self.path = None;
    }

    /// Records a turn on `model`.
    pub fn count(&mut self, model: &str) {
        *self.turns.entry(model.to_string()).or_default() += 1;
        *self.unsaved.entry(model.to_string()).or_default() += 1;
    }

    /// The models that ran a turn, most used first, then by name.
    pub fn ranked(&self) -> Vec<(&str, u64)> {
        let mut ranked: Vec<(&str, u64)> = self
            .turns
            .iter()
            .filter(|(_, turns)| **turns > 0)
            .map(|(model, turns)| (model.as_str(), *turns))
            .collect();
        ranked.sort_by_key(|(_, turns)| Reverse(*turns));
        ranked
    }
}

/// Adds `turns` to the counts saved at `path`, reading them again first so
/// that another instance's saves since this one loaded are kept. Written
/// aside and renamed over, so a reader never sees half a file. Two saves at
/// the same moment can still lose one's turns; the counts only order the
/// picker, which a lost turn barely moves.
pub async fn add(path: &Path, turns: Turns) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut saved: Turns = match tokio::fs::read_to_string(path).await {
        Ok(text) => serde_json::from_str(&text).map_err(io::Error::other)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => Turns::new(),
        Err(e) => return Err(e),
    };
    for (model, n) in turns {
        *saved.entry(model).or_default() += n;
    }
    let json = serde_json::to_string_pretty(&saved).map_err(io::Error::other)?;
    let aside = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let written = match tokio::fs::write(&aside, json).await {
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

    fn usage(turns: &[(&str, u64)]) -> LlmUsage {
        let mut usage = LlmUsage::default();
        for (model, n) in turns {
            for _ in 0..*n {
                usage.count(model);
            }
        }
        usage
    }

    #[test]
    fn ranked_breaks_ties_by_name() {
        let usage = usage(&[("b", 2), ("a", 2), ("c", 5)]);
        assert_eq!(usage.ranked(), [("c", 5), ("a", 2), ("b", 2)]);
    }

    #[tokio::test]
    async fn round_trips_through_its_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("llm-usage.json");

        let missing = LlmUsage::load(path.clone()).await;
        assert_eq!(missing.saved_at(), Some(path.as_path()));

        let mut usage = usage(&[("glm", 2)]);
        add(&path, usage.take_unsaved()).await.expect("saves");
        assert_eq!(LlmUsage::load(path.clone()).await.ranked(), [("glm", 2)]);
        assert!(usage.take_unsaved().is_empty(), "taken once");
    }

    #[tokio::test]
    async fn a_damaged_file_is_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("llm-usage.json");
        std::fs::write(&path, "not json").expect("write");

        let usage = LlmUsage::load(path).await;
        assert_eq!(usage.saved_at(), None);
        assert!(usage.ranked().is_empty());
    }
}
