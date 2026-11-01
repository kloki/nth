//! The host ID Sign in with ChatGPT asks for: one per installation, the
//! same across sign-ins and account switches. OpenAI issues the client ID,
//! nth makes this one, and it is kept beside `auth.json` so signing out
//! does not lose it.

use std::io;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::{Error, Store};

#[derive(Deserialize, Serialize)]
struct Host {
    id: String,
}

/// This installation's host ID, made on first use.
pub(crate) async fn id(store: &Store) -> Result<String, Error> {
    let path = store.path().with_file_name("chatgpt-host.json");
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            if let Ok(host) = serde_json::from_slice::<Host>(&bytes)
                && !host.id.trim().is_empty()
            {
                return Ok(host.id);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(source) => return Err(Error::Io { path, source }),
    }
    let id = uuid()?;
    let bytes = serde_json::to_vec(&Host { id: id.clone() }).map_err(|source| Error::Json {
        path: path.clone(),
        source,
    })?;
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|source| Error::Io {
                path: dir.to_path_buf(),
                source,
            })?;
    }
    match create(&path, &bytes).await {
        Ok(()) => Ok(id),
        // Another nth made one first: use its, so both send the same host.
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let bytes = tokio::fs::read(&path).await.map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            let host: Host = serde_json::from_slice(&bytes).map_err(|source| Error::Json {
                path: path.clone(),
                source,
            })?;
            Ok(host.id)
        }
        Err(source) => Err(Error::Io { path, source }),
    }
}

async fn create(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    file.write_all(bytes).await?;
    file.sync_all().await
}

/// A random version 4 UUID, as `urn:uuid:…`; the form OpenAI's guide uses.
fn uuid() -> Result<String, Error> {
    let mut bytes = [0; 16];
    getrandom::fill(&mut bytes).map_err(Error::Random)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn one_id_per_installation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("nth/auth.json"));

        let first = id(&store).await.unwrap();
        assert!(first.starts_with("urn:uuid:"), "{first}");
        assert_eq!(id(&store).await.unwrap(), first);

        let path = dir.path().join("nth/chatgpt-host.json");
        let saved: Host = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved.id, first);
    }
}
