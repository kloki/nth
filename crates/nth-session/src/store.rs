//! Saved sessions: one JSON file per session, listed newest first. Sessions
//! are not tied to the directory they ran in; the list is ordered by time.

use std::{
    io,
    path::{Path, PathBuf},
    time::SystemTime,
};

use uuid::Uuid;

use crate::Session;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("neither XDG_DATA_HOME nor HOME is set, so there is nowhere to keep sessions")]
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

#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// The store under `$XDG_DATA_HOME/nth/sessions`, or
    /// `~/.local/share/nth/sessions` when that is unset.
    pub fn open() -> Result<Self, Error> {
        let data = match std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
            Some(data) => PathBuf::from(data),
            None => PathBuf::from(std::env::var_os("HOME").ok_or(Error::NoDataDir)?)
                .join(".local/share"),
        };
        Ok(Self::at(data.join("nth/sessions")))
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
        let json = serde_json::to_vec_pretty(session).map_err(|source| Error::Json {
            path: self.path(session.id),
            source,
        })?;
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
        .filter_map(|json| serde_json::from_slice::<Session>(&json).ok())
        .map(|session| Summary {
            id: session.id,
            title: session.title().unwrap_or_default().to_string(),
            cwd: session.cwd,
            model: session.model,
            updated_at: session.updated_at,
        })
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
    Ok(sessions)
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
