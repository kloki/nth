//! Title metadata shared by session owners and the store.

use serde::{Deserialize, Serialize};

mod generation;
pub use generation::{Job, Outcome};

/// Shared title state for one live session. Operations are serialized with
/// one another, while the store serializes them with other processes' saves.
#[derive(Debug, Clone)]
pub struct Owner {
    id: uuid::Uuid,
    store: Option<crate::Store>,
    state: std::sync::Arc<tokio::sync::Mutex<Option<Metadata>>>,
}

impl Owner {
    pub fn new(session: &crate::Session, store: Option<crate::Store>) -> Self {
        Self {
            id: session.id,
            store,
            state: std::sync::Arc::new(tokio::sync::Mutex::new(session.title_metadata.clone())),
        }
    }

    pub async fn metadata(&self) -> Option<Metadata> {
        self.state.lock().await.clone()
    }

    pub fn snapshot(&self) -> Option<Metadata> {
        self.state.try_lock().ok().and_then(|state| state.clone())
    }

    /// Refresh from disk when a save exists, leaving an unsaved owner alone.
    pub async fn refresh(&self) -> Result<Option<Metadata>, crate::store::Error> {
        let mut state = self.state.lock().await;
        if let Some(store) = &self.store {
            match store.title_metadata(self.id).await {
                Ok(metadata) => {
                    let replace = match (&*state, &metadata) {
                        (Some(local), Some(saved)) => {
                            saved
                                .title
                                .as_ref()
                                .is_some_and(|title| title.origin == Origin::User)
                                || saved.revision >= local.revision
                        }
                        (None, _) => true,
                        _ => false,
                    };
                    if replace {
                        *state = metadata;
                    }
                }
                Err(crate::store::Error::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(state.clone())
    }

    /// Rename the saved metadata, or hold the rename locally before the
    /// first save. The store never writes a copy of the conversation.
    pub async fn rename(&self, text: &str) -> Result<Metadata, crate::store::Error> {
        let text = validate(text)?;
        let mut state = self.state.lock().await;
        if let Some(store) = &self.store {
            match store.rename(self.id, &text).await {
                Ok(metadata) => {
                    *state = Some(metadata.clone());
                    return Ok(metadata);
                }
                Err(crate::store::Error::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        let metadata = state.get_or_insert_with(Metadata::legacy);
        metadata.rename(&text)?;
        Ok(metadata.clone())
    }

    /// Adopt an owner's result only when it is at least as recent as the
    /// current snapshot; a pending local rename must not be displaced.
    pub async fn adopt(&self, metadata: Option<Metadata>) {
        let mut state = self.state.lock().await;
        let newer = match (&*state, &metadata) {
            (Some(old), Some(new)) => {
                let user = |metadata: &Metadata| {
                    metadata
                        .title
                        .as_ref()
                        .is_some_and(|t| t.origin == Origin::User)
                };
                (!user(old) || user(new)) && new.revision > old.revision
            }
            (None, Some(_)) => true,
            _ => false,
        };
        if newer {
            *state = metadata;
        }
    }

    pub async fn generated(&self, expected: u64, text: &str) {
        let _ = self.refresh().await;
        let mut state = self.state.lock().await;
        if let Some(metadata) = state.as_mut() {
            let _ = metadata.generated(expected, text);
        }
    }
}

/// A main session's metadata-only rename tool.
pub struct Rename(pub Owner);

impl nth_protocol::Tool for Rename {
    fn spec(&self) -> nth_protocol::ToolSpec {
        nth_protocol::ToolSpec {
            name: "session_rename",
            description: "Rename the current session with a short descriptive title.".into(),
            parameters: serde_json::json!({
                "type": "object", "properties": {"title": {"type": "string"}},
                "required": ["title"], "additionalProperties": false
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a nth_protocol::ToolContext,
    ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
        Box::pin(async move {
            if ctx.is_subagent {
                return Err("subagents cannot rename sessions".into());
            }
            let title = args
                .get("title")
                .and_then(|value| value.as_str())
                .ok_or("title is required")?;
            let metadata = self.0.rename(title).await.map_err(|e| e.to_string())?;
            Ok(format!(
                "Session renamed: {}",
                metadata.title.expect("rename sets title").text
            ))
        })
    }
}

/// A title's provenance. A user title always wins over generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Generated,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Title {
    pub text: String,
    pub origin: Origin,
}

/// Whether the first eligible prompt still has an automatic attempt available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Generation {
    Pending,
    Attempted,
}

/// `None` on a session means a legacy save; it must not be auto-titled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    pub revision: u64,
    pub title: Option<Title>,
    pub generation: Generation,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            revision: 0,
            title: None,
            generation: Generation::Pending,
        }
    }
}

impl Metadata {
    /// Legacy sessions have already missed their initial generation opportunity.
    pub fn legacy() -> Self {
        Self {
            generation: Generation::Attempted,
            ..Self::default()
        }
    }

    pub fn rename(&mut self, text: &str) -> Result<(), Error> {
        let text = validate(text)?;
        self.advance()?;
        self.title = Some(Title {
            text,
            origin: Origin::User,
        });
        self.generation = Generation::Attempted;
        Ok(())
    }

    /// Apply an automatic result only against the revision it was started on.
    pub fn generated(&mut self, expected: u64, text: &str) -> Result<bool, Error> {
        if self.revision != expected
            || self
                .title
                .as_ref()
                .is_some_and(|t| t.origin == Origin::User)
        {
            return Ok(false);
        }
        let text = validate(text)?;
        self.advance()?;
        self.title = Some(Title {
            text,
            origin: Origin::Generated,
        });
        self.generation = Generation::Attempted;
        Ok(true)
    }

    /// Record an attempt before launching it, even if it later fails.
    pub fn attempted(&mut self) -> Result<(), Error> {
        if self.generation == Generation::Pending {
            self.advance()?;
            self.generation = Generation::Attempted;
        }
        Ok(())
    }

    fn advance(&mut self) -> Result<(), Error> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(Error::RevisionExhausted)?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("title must not be empty")]
    Empty,
    #[error("title must be a single line without control characters")]
    Control,
    #[error("title must be at most 100 characters")]
    TooLong,
    #[error("title metadata revision exhausted")]
    RevisionExhausted,
}

/// Trim a manual title and reject invalid input without silently truncating it.
pub fn validate(text: &str) -> Result<String, Error> {
    if text.chars().any(char::is_control) {
        return Err(Error::Control);
    }
    let text = text.trim();
    if text.is_empty() {
        return Err(Error::Empty);
    }
    if text.chars().count() > 100 {
        return Err(Error::TooLong);
    }
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use nth_protocol::Tool;

    use super::*;

    #[tokio::test]
    async fn live_rename_beats_generation_and_refreshes_external_changes() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::Store::at(dir.path());
        let mut session = crate::Session::new("model", dir.path().into());
        let owner = Owner::new(&session, Some(store.clone()));
        let job = Job::admit(&mut session, "hello").unwrap().unwrap();
        owner.adopt(session.title_metadata.clone()).await;
        owner.rename("manual").await.unwrap();
        owner.generated(job.expected_revision, "late").await;
        session.title_metadata = owner.metadata().await;
        session
            .messages
            .push(nth_protocol::Message::User("hello".into()));
        store.save(&session).await.unwrap();
        assert_eq!(
            store.load(session.id).await.unwrap().title().as_deref(),
            Some("manual")
        );
        store.rename(session.id, "external").await.unwrap();
        owner.refresh().await.unwrap();
        assert_eq!(owner.snapshot().unwrap().title.unwrap().text, "external");
    }

    #[tokio::test]
    async fn rename_tool_is_bound_to_main_owner() {
        let session = crate::Session::new("model", ".".into());
        let owner = Owner::new(&session, None);
        let tool = Rename(owner.clone());
        let mut ctx = nth_protocol::ToolContext::new(".".into());
        ctx.is_subagent = true;
        assert!(
            tool.call(serde_json::json!({"title":"child"}), &ctx)
                .await
                .is_err()
        );
        ctx.is_subagent = false;
        assert!(
            tool.call(serde_json::json!({"title":"main"}), &ctx)
                .await
                .is_ok()
        );
        assert_eq!(owner.snapshot().unwrap().title.unwrap().text, "main");
    }

    #[test]
    fn validates_manual_titles_and_rejects_stale_generation() {
        assert_eq!(validate("  résumé  ").unwrap(), "résumé");
        assert_eq!(validate(" "), Err(Error::Empty));
        assert_eq!(validate("a\nb"), Err(Error::Control));
        assert_eq!(validate(&"é".repeat(101)), Err(Error::TooLong));
        let mut metadata = Metadata::default();
        metadata.attempted().unwrap();
        let revision = metadata.revision;
        metadata.rename("mine").unwrap();
        assert!(!metadata.generated(revision, "late").unwrap());
        assert_eq!(metadata.title.unwrap().text, "mine");
    }
}
