//! Saved sessions: one JSON file per session, listed newest first. Sessions
//! are not tied to the directory they ran in; the list is ordered by time.

use std::{
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use serde::Deserialize;
use uuid::Uuid;

use crate::{Ledger, Session};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("neither XDG_DATA_HOME nor HOME is set, so nth has nowhere to save")]
    NoDataDir,
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("{}: {source}", path.display())]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("listing sessions failed: {0}")]
    Listing(#[from] tokio::task::JoinError),
}

/// What the resume picker shows of a saved session.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub id: Uuid,
    /// First line of the first prompt.
    pub title: String,
    pub cwd: PathBuf,
    pub model: String,
    pub updated_at: SystemTime,
}

/// What [`list`] reads of a save: the fields the picker shows, which skips
/// the history. The title is saved beside the session so the list never
/// parses one; saves from before it carry none and are parsed whole.
#[derive(Deserialize)]
struct Head {
    id: Uuid,
    cwd: PathBuf,
    model: String,
    updated_at: SystemTime,
    title: Option<String>,
}

/// Where nth keeps what it saves: `$XDG_DATA_HOME/nth`, or
/// `~/.local/share/nth` when that is unset.
pub fn data_dir() -> Result<PathBuf, Error> {
    let data = match std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        Some(data) => PathBuf::from(data),
        None => {
            PathBuf::from(std::env::var_os("HOME").ok_or(Error::NoDataDir)?).join(".local/share")
        }
    };
    Ok(data.join("nth"))
}

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// The store under `$XDG_DATA_HOME/nth/sessions`, or
    /// `~/.local/share/nth/sessions` when that is unset.
    pub fn open() -> Result<Self, Error> {
        Ok(Self::at(data_dir()?.join("sessions")))
    }

    pub fn at(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// Writes `session`, replacing its earlier save. A session nothing was
    /// asked in yet is not worth keeping, so it is skipped.
    pub async fn save(&self, session: &Session) -> Result<(), Error> {
        if session.is_empty() {
            return Ok(());
        }
        let json_error = |source| Error::Json {
            path: self.path(session.id),
            source,
        };
        let mut json = serde_json::to_value(session).map_err(json_error)?;
        // The title goes beside the session for [`Head`]; loading ignores it.
        if let Some(fields) = json.as_object_mut() {
            fields.insert("title".into(), session.title().into());
        }
        let json = serde_json::to_vec_pretty(&json).map_err(json_error)?;
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(io_at(&self.dir))?;
        // Written aside and renamed over, so a crash mid-write never leaves
        // a half file in place of the last good save.
        let path = self.path(session.id);
        let tmp = path.with_extension("json.tmp");
        tokio::fs::write(&tmp, json).await.map_err(io_at(&tmp))?;
        tokio::fs::rename(&tmp, &path).await.map_err(io_at(&path))
    }

    /// Every saved session, the most recently used first. Files that don't
    /// parse, from a newer nth say, are left out rather than failing the list.
    pub async fn list(&self) -> Result<Vec<Summary>, Error> {
        let dir = self.dir.clone();
        // Parsing every session is CPU work, too much for the runtime.
        tokio::task::spawn_blocking(move || list(&dir)).await?
    }

    /// What every saved session spent, newest first; the histories are
    /// skipped.
    pub async fn spending(&self) -> Result<Vec<Spending>, Error> {
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || spending(&dir)).await?
    }

    pub async fn load(&self, id: Uuid) -> Result<Session, Error> {
        let path = self.path(id);
        let json = tokio::fs::read(&path).await.map_err(io_at(&path))?;
        serde_json::from_slice(&json).map_err(|source| Error::Json { path, source })
    }

    /// The session used last, if any was saved.
    pub async fn latest(&self) -> Result<Option<Session>, Error> {
        match self.list().await?.first() {
            Some(summary) => self.load(summary.id).await.map(Some),
            None => Ok(None),
        }
    }

    fn path(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
}

fn list(dir: &Path) -> Result<Vec<Summary>, Error> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // Nothing saved yet.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_at(dir)(e)),
    };
    let mut sessions: Vec<Summary> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| std::fs::read(&path).ok())
        .filter_map(|json| summary(&json))
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    Ok(sessions)
}

/// What a saved session spent, without its history.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Spending {
    pub id: Uuid,
    pub updated_at: SystemTime,
    #[serde(default)]
    pub usage: Ledger,
}

fn spending(dir: &Path) -> Result<Vec<Spending>, Error> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_at(dir)(e)),
    };
    let mut sessions: Vec<Spending> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| std::fs::read(&path).ok())
        .filter_map(|json| serde_json::from_slice(&json).ok())
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    Ok(sessions)
}

fn summary(json: &[u8]) -> Option<Summary> {
    let head: Head = serde_json::from_slice(json).ok()?;
    let title = match head.title {
        Some(title) => title,
        // Saved before the title was kept beside the session.
        None => serde_json::from_slice::<Session>(json)
            .ok()?
            .title()
            .unwrap_or_default(),
    };
    Some(Summary {
        id: head.id,
        title,
        cwd: head.cwd,
        model: head.model,
        updated_at: head.updated_at,
    })
}

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> Error {
    let path = path.to_path_buf();
    move |source| Error::Io { path, source }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nth_protocol::Message;

    use super::*;

    fn session(prompt: &str, at_secs: u64) -> Session {
        let mut session = Session::new("glm-5.3", "/repo".into());
        session.messages.push(Message::User(prompt.into()));
        session.updated_at = SystemTime::UNIX_EPOCH + Duration::from_secs(at_secs);
        session
    }

    #[tokio::test]
    async fn reads_what_every_session_spent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path().join("sessions"));
        let mut spent = session("fix the build", 2);
        let turn = spent.usage.begin("glm-5.3", None);
        spent.usage.add(
            turn,
            nth_protocol::Usage {
                input: 10,
                ..Default::default()
            },
        );
        store.save(&spent).await.expect("saves");
        store.save(&session("older", 1)).await.expect("saves");

        let spending = store.spending().await.expect("reads");
        assert_eq!(spending.len(), 2);
        assert_eq!(spending[0].id, spent.id, "newest first");
        assert_eq!(spending[0].usage, spent.usage);
        assert!(spending[1].usage.spends().is_empty());
    }

    #[tokio::test]
    async fn lists_saved_sessions_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path().join("sessions"));
        let old = session("fix the build\nplease", 1);
        let new = session("add resume", 2);
        store.save(&old).await.expect("saves");
        store.save(&new).await.expect("saves");

        let titles: Vec<_> = store
            .list()
            .await
            .expect("lists")
            .into_iter()
            .map(|s| s.title)
            .collect();

        assert_eq!(titles, ["add resume", "fix the build"]);
        assert_eq!(store.latest().await.expect("loads"), Some(new));
        assert_eq!(store.load(old.id).await.expect("loads"), old);
    }

    #[tokio::test]
    async fn a_save_from_before_titles_still_lists_by_its_prompt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path());
        let old = session("fix the build", 1);
        let json = serde_json::to_vec(&old).expect("serializes");
        assert!(!json.windows(7).any(|w| w == b"\"title\""));
        std::fs::write(dir.path().join(format!("{}.json", old.id)), json).expect("writes");

        let listed = store.list().await.expect("lists");

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].title, "fix the build");
        assert_eq!(listed[0].id, old.id);
    }

    #[tokio::test]
    async fn skips_empty_sessions_and_unreadable_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path());
        store
            .save(&Session::new("glm-5.3", "/repo".into()))
            .await
            .expect("saves");
        std::fs::write(dir.path().join("broken.json"), "{").expect("writes");

        assert_eq!(store.list().await.expect("lists"), []);
        assert_eq!(store.latest().await.expect("loads"), None);
    }

    #[tokio::test]
    async fn a_missing_directory_is_an_empty_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path().join("nope"));

        assert_eq!(store.list().await.expect("lists"), []);
    }
}
