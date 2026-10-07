//! How many turns ran on each model, kept across runs in one JSON file:
//! the picker lists the most used first, and the diagnostics tab graphs it.

use std::{
    cmp::Reverse,
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use nth_protocol::ModelInfo;

#[derive(Debug, Default)]
pub struct LlmUsage {
    turns: BTreeMap<String, u64>,
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

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&self.turns).unwrap_or_default()
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
    }

    /// Most used first, keeping each provider's models together for the
    /// picker's groups: the providers with the most turns first, then the
    /// most used of each. The sort is stable, so ties keep the listing's
    /// order.
    pub fn order(&self, models: &mut [ModelInfo]) {
        let turns = |m: &ModelInfo| self.turns.get(&m.id).copied().unwrap_or(0);
        // Per provider: its turns, and where the listing first has it.
        let mut providers: Vec<(Option<String>, u64, usize)> = Vec::new();
        for (i, model) in models.iter().enumerate() {
            let origin = model.origin.as_ref().map(|o| o.id.clone());
            match providers.iter_mut().find(|(o, ..)| *o == origin) {
                Some((_, total, _)) => *total += turns(model),
                None => providers.push((origin, turns(model), i)),
            }
        }
        models.sort_by_cached_key(|m| {
            let origin = m.origin.as_ref().map(|o| o.id.as_str());
            let (_, total, first) = providers
                .iter()
                .find(|(o, ..)| o.as_deref() == origin)
                .cloned()
                .unwrap_or_default();
            (Reverse(total), first, Reverse(turns(m)))
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm_picker::tests::model;

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
    fn the_most_used_come_first_and_ties_keep_their_order() {
        let mut models = vec![
            model("a", false),
            model("b", false),
            model("c", false),
            model("d", false),
        ];
        usage(&[("c", 3), ("d", 1)]).order(&mut models);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["c", "d", "a", "b"]);
    }

    fn served(provider: &str, id: &str) -> ModelInfo {
        ModelInfo {
            id: format!("{provider}/{id}"),
            origin: Some(nth_protocol::Origin {
                id: provider.into(),
                name: provider.into(),
            }),
            ..model(id, false)
        }
    }

    #[test]
    fn each_provider_stays_together_the_most_used_first() {
        let mut models = vec![
            served("go", "a"),
            served("go", "b"),
            served("ly", "c"),
            served("ly", "d"),
            served("zen", "e"),
        ];
        usage(&[("go/b", 1), ("ly/d", 3), ("zen/e", 1)]).order(&mut models);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["ly/d", "ly/c", "go/b", "go/a", "zen/e"]);
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

        let usage = usage(&[("glm", 2)]);
        std::fs::write(&path, usage.to_json()).expect("write");
        assert_eq!(LlmUsage::load(path.clone()).await.ranked(), [("glm", 2)]);
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
