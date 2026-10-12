//! `auth.json`: one entry per provider id, as opencode keeps it. The file
//! holds secrets, so it is only ever written whole, aside and renamed over,
//! readable by you alone.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Error;

/// Tokens from an OAuth login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OAuth {
    pub access: String,
    pub refresh: String,
    /// When `access` stops working, in unix milliseconds.
    pub expires: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
}

impl OAuth {
    /// Whether `access` runs out within `margin_ms`, so a request sent now
    /// might reach the server with a dead token.
    pub fn expires_within(&self, margin_ms: u64) -> bool {
        self.expires < now_ms() + margin_ms
    }
}

/// An entry as written, tagged by kind so API keys can sit beside OAuth
/// tokens later.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Entry {
    Oauth(OAuth),
}

#[derive(Debug, Clone)]
pub struct Store {
    path: PathBuf,
}

impl Store {
    /// `auth.json` in `$XDG_DATA_HOME/nth`, or `~/.local/share/nth` when
    /// that is unset, beside the saved sessions.
    pub fn new() -> Result<Self, Error> {
        let data = match std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
            Some(data) => PathBuf::from(data),
            None => PathBuf::from(std::env::var_os("HOME").ok_or(Error::NoDataDir)?)
                .join(".local/share"),
        };
        Ok(Self::at(data.join("nth/auth.json")))
    }

    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `id`'s tokens, if it has any.
    pub async fn oauth(&self, id: &str) -> Result<Option<OAuth>, Error> {
        let entries = self.parse(read(&self.path, tokio::fs::read(&self.path).await)?)?;
        Ok(oauth(entries, id))
    }

    /// As `oauth`, without a runtime: for questions asked while drawing.
    pub fn oauth_blocking(&self, id: &str) -> Result<Option<OAuth>, Error> {
        let entries = self.parse(read(&self.path, std::fs::read(&self.path))?)?;
        Ok(oauth(entries, id))
    }

    pub async fn save(&self, id: &str, auth: &OAuth) -> Result<(), Error> {
        let entry = Entry::Oauth(auth.clone());
        let entry = serde_json::to_value(entry).map_err(|source| self.json_error(source))?;
        self.update(|entries| {
            entries.insert(id.to_string(), entry);
        })
        .await
    }

    pub async fn remove(&self, id: &str) -> Result<(), Error> {
        self.update(|entries| {
            entries.remove(id);
        })
        .await
    }

    /// Rewrites the file with `change` made, keeping the entries nth does not
    /// know (another kind, or written by a newer nth).
    async fn update(&self, change: impl FnOnce(&mut BTreeMap<String, Value>)) -> Result<(), Error> {
        let mut entries = self.parse(read(&self.path, tokio::fs::read(&self.path).await)?)?;
        change(&mut entries);
        let json = serde_json::to_vec_pretty(&entries).map_err(|source| self.json_error(source))?;
        if let Some(dir) = self.path.parent() {
            tokio::fs::create_dir_all(dir).await.map_err(io_at(dir))?;
        }
        let tmp = self
            .path
            .with_extension(format!("json.{}.tmp", std::process::id()));
        write_private(&tmp, &json).await.map_err(io_at(&tmp))?;
        tokio::fs::rename(&tmp, &self.path)
            .await
            .map_err(io_at(&self.path))
    }

    fn parse(&self, bytes: Option<Vec<u8>>) -> Result<BTreeMap<String, Value>, Error> {
        match bytes {
            Some(bytes) => serde_json::from_slice(&bytes).map_err(|source| self.json_error(source)),
            None => Ok(BTreeMap::new()),
        }
    }

    fn json_error(&self, source: serde_json::Error) -> Error {
        Error::Json {
            path: self.path.clone(),
            source,
        }
    }
}

/// An entry of a kind this nth does not read counts as no tokens.
fn oauth(mut entries: BTreeMap<String, Value>, id: &str) -> Option<OAuth> {
    match serde_json::from_value(entries.remove(id)?) {
        Ok(Entry::Oauth(auth)) => Some(auth),
        Err(_) => None,
    }
}

/// The file's bytes, or none when there is no file yet.
fn read(path: &Path, read: io::Result<Vec<u8>>) -> Result<Option<Vec<u8>>, Error> {
    match read {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(Error::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

async fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use tokio::io::AsyncWriteExt;

    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    file.write_all(bytes).await?;
    file.sync_all().await
}

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> Error + '_ {
    move |source| Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tokens() -> OAuth {
        OAuth {
            access: "access".into(),
            refresh: "refresh".into(),
            expires: 1_700_000_000_000,
            account_id: Some("acct".into()),
        }
    }

    #[tokio::test]
    async fn saved_tokens_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("nth/auth.json"));
        assert_eq!(store.oauth("chatgpt").await.unwrap(), None);

        store.save("chatgpt", &tokens()).await.unwrap();
        assert_eq!(store.oauth("chatgpt").await.unwrap(), Some(tokens()));
        assert_eq!(store.oauth_blocking("chatgpt").unwrap(), Some(tokens()));
        assert_eq!(store.oauth("other").await.unwrap(), None);

        store.remove("chatgpt").await.unwrap();
        assert_eq!(store.oauth("chatgpt").await.unwrap(), None);
    }

    #[tokio::test]
    async fn file_is_opencodes_shape_and_keeps_unknown_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, r#"{"anthropic":{"type":"api","key":"sk"}}"#).unwrap();
        let store = Store::at(&path);
        assert_eq!(store.oauth("anthropic").await.unwrap(), None);

        store.save("chatgpt", &tokens()).await.unwrap();
        let json: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "anthropic": {"type": "api", "key": "sk"},
                "chatgpt": {
                    "type": "oauth",
                    "access": "access",
                    "refresh": "refresh",
                    "expires": 1_700_000_000_000u64,
                    "account_id": "acct",
                },
            })
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn only_you_can_read_it() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        store.save("chatgpt", &tokens()).await.unwrap();
        let mode = std::fs::metadata(store.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
