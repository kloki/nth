//! Saved sessions: one JSON file per session, listed newest first. Sessions
//! are not tied to the directory they ran in; the list is ordered by time.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use base64::Engine;
use nth_context::Paths;
use nth_protocol::{Image, MAX_IMAGE_BYTES, Message};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{Ledger, Session, compaction};

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
    /// Where it started, when it moved into a worktree.
    pub origin: Option<PathBuf>,
    pub model: String,
    pub updated_at: SystemTime,
}

/// How long ago `then` was, in its largest whole unit, as the resume
/// picker and `nth session list` say it: `now`, `5m`, `3h`, `2d`.
pub fn age(now: SystemTime, then: SystemTime) -> String {
    let secs = now.duration_since(then).unwrap_or(Duration::ZERO).as_secs();
    match secs {
        0..60 => "now".into(),
        60..3_600 => format!("{}m", secs / 60),
        3_600..86_400 => format!("{}h", secs / 3_600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// What [`list`] reads of a save: the fields the picker shows, which skips
/// the history. The title is saved beside the session so the list never
/// parses one; saves from before it carry none and are parsed whole.
#[derive(Deserialize)]
struct Head {
    id: Uuid,
    cwd: PathBuf,
    #[serde(default)]
    origin: Option<PathBuf>,
    model: String,
    updated_at: SystemTime,
    title: Option<String>,
}

/// Where nth keeps what it saves: `$XDG_DATA_HOME/nth`, or
/// `~/.local/share/nth` when that is unset.
pub fn data_dir() -> Result<PathBuf, Error> {
    Paths::from_env().nth_data().ok_or(Error::NoDataDir)
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
        tokio::fs::File::open(&tmp)
            .await
            .map_err(io_at(&tmp))?
            .sync_all()
            .await
            .map_err(io_at(&tmp))?;
        tokio::fs::rename(&tmp, &path).await.map_err(io_at(&path))?;
        tokio::fs::File::open(&self.dir)
            .await
            .map_err(io_at(&self.dir))?
            .sync_all()
            .await
            .map_err(io_at(&self.dir))?;
        Ok(())
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
        let mut session: Session =
            serde_json::from_slice(&json).map_err(|source| Error::Json { path, source })?;
        load_images(&mut session.messages, &self.images()).await;
        Ok(session)
    }

    /// The folder every session's image files live under, one folder per
    /// session; a subagent's go in its parent's.
    pub fn images(&self) -> PathBuf {
        self.dir.join("images")
    }

    /// Removes `id`'s save; one that was never saved is an error.
    pub async fn delete(&self, id: Uuid) -> Result<(), Error> {
        let path = self.path(id);
        tokio::fs::remove_file(&path).await.map_err(io_at(&path))
    }

    /// The session used last, if any was saved.
    pub async fn latest(&self) -> Result<Option<Session>, Error> {
        match self.list().await?.first() {
            Some(summary) => self.load(summary.id).await.map(Some),
            None => Ok(None),
        }
    }

    /// Where the read tool keeps the images `id` and its subagents showed
    /// the model: beside the saves, so a store in a test keeps its own, and
    /// under `images`, the only folder a load reads them back from.
    pub fn images_dir(&self, id: Uuid) -> PathBuf {
        self.images().join(id.to_string())
    }

    fn path(&self, id: Uuid) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }
}

/// Reads the bytes of every image the tools showed, which the save leaves
/// out, once, so the requests of a resumed session need not. Only images a
/// request still carries are read, those after the last compaction, and an
/// image shown twice is held once. An image whose file is gone, or is not
/// one the store wrote, stays without them, and its requests carry a note
/// in its place.
async fn load_images(messages: &mut [Message], root: &Path) {
    // No folder, no images to read.
    let Ok(root) = tokio::fs::canonicalize(root).await else {
        return;
    };
    let start = compaction::last_marker(messages).map_or(0, |at| at + 1);
    let mut read: HashMap<String, Arc<str>> = HashMap::new();
    for message in &mut messages[start..] {
        let Message::ToolResult { images, .. } = message else {
            continue;
        };
        for image in images {
            if let Some(data) = read.get(&image.sha256) {
                image.data = Some(data.clone());
            } else if let Some(bytes) = image_bytes(image, &root).await {
                let data: Arc<str> = base64::engine::general_purpose::STANDARD
                    .encode(bytes)
                    .into();
                read.insert(image.sha256.clone(), data.clone());
                image.data = Some(data);
            }
        }
    }
}

/// The bytes of `image`'s file, when it is one the store wrote: a plain
/// file under `root` (the session JSON names the path, and anything that
/// edits it must not get another file sent to a provider), within the cap,
/// and the bytes the image's hash names.
async fn image_bytes(image: &Image, root: &Path) -> Option<Vec<u8>> {
    let meta = tokio::fs::symlink_metadata(&image.path).await.ok()?;
    if !meta.is_file() || meta.len() > MAX_IMAGE_BYTES as u64 {
        return None;
    }
    let path = tokio::fs::canonicalize(&image.path).await.ok()?;
    if !path.starts_with(root) {
        return None;
    }
    let bytes = tokio::fs::read(&path).await.ok()?;
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    (sha256 == image.sha256 && bytes.len() <= MAX_IMAGE_BYTES).then_some(bytes)
}

/// What `parse` reads out of every saved session in `dir`; one it cannot
/// read is left out.
fn scan<T>(dir: &Path, parse: impl Fn(&[u8]) -> Option<T>) -> Result<Vec<T>, Error> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // Nothing saved yet.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(io_at(dir)(e)),
    };
    Ok(entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| std::fs::read(&path).ok())
        .filter_map(|json| parse(&json))
        .collect())
}

fn list(dir: &Path) -> Result<Vec<Summary>, Error> {
    let mut sessions = scan(dir, summary)?;
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
    let mut sessions: Vec<Spending> = scan(dir, |json| serde_json::from_slice(json).ok())?;
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
        origin: head.origin,
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
    use nth_protocol::Message;

    use super::*;

    fn session(prompt: &str, at_secs: u64) -> Session {
        let mut session = Session::new("glm-5.3", "/repo".into());
        session.messages.push(Message::User(prompt.into()));
        session.updated_at = SystemTime::UNIX_EPOCH + Duration::from_secs(at_secs);
        session
    }

    /// An image of `bytes` at `path`, written there, as the store's
    /// session would show it.
    fn shown(path: PathBuf, bytes: &[u8]) -> Image {
        std::fs::create_dir_all(path.parent().expect("a folder")).expect("creates");
        std::fs::write(&path, bytes).expect("writes");
        Image {
            mime: "image/png".into(),
            sha256: format!("{:x}", Sha256::digest(bytes)),
            width: 1,
            height: 1,
            path,
            data: None,
        }
    }

    fn result(images: Vec<Image>) -> Message {
        Message::ToolResult {
            call_id: "1".into(),
            content: "Image read".into(),
            images,
        }
    }

    /// The images of the tool results in `session` once saved and loaded.
    async fn loaded(store: &Store, session: &Session) -> Vec<Image> {
        store.save(session).await.expect("saves");
        let loaded = store.load(session.id).await.expect("loads");
        loaded
            .messages
            .into_iter()
            .flat_map(|m| match m {
                Message::ToolResult { images, .. } => images,
                _ => Vec::new(),
            })
            .collect()
    }

    fn data(images: &[Image]) -> Vec<Option<&str>> {
        images.iter().map(|i| i.data.as_deref()).collect()
    }

    #[tokio::test]
    async fn loading_reads_the_images_the_save_left_out() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path().join("sessions"));
        let folder = store.images().join("parent");
        let kept = shown(folder.join("kept.png"), b"png");
        let gone = shown(folder.join("gone.png"), b"gif");
        std::fs::remove_file(&gone.path).expect("removes");
        let mut session = session("look", 1);
        session.messages.push(result(vec![kept, gone]));

        let images = loaded(&store, &session).await;
        assert_eq!(
            data(&images),
            [Some("cG5n"), None],
            "the gone file's stays empty"
        );
    }

    #[tokio::test]
    async fn only_the_stores_own_image_files_are_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path().join("sessions"));
        let folder = store.images().join("parent");
        let outside = shown(dir.path().join("secret.png"), b"key");
        let mut wrong_hash = shown(folder.join("edited.png"), b"png");
        wrong_hash.sha256 = format!("{:x}", Sha256::digest(b"other"));
        let mut link = shown(dir.path().join("target.png"), b"key");
        let link_path = folder.join("link.png");
        std::os::unix::fs::symlink(&link.path, &link_path).expect("links");
        link.path = link_path;
        let escape = Image {
            path: folder.join("../../../secret.png"),
            ..outside.clone()
        };
        let mut session = session("look", 1);
        session
            .messages
            .push(result(vec![outside, wrong_hash, link, escape]));

        let images = loaded(&store, &session).await;
        assert_eq!(data(&images), [None, None, None, None]);
    }

    #[tokio::test]
    async fn an_image_shown_twice_is_held_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path().join("sessions"));
        let image = shown(store.images().join("parent/a.png"), b"png");
        let mut session = session("look", 1);
        session.messages.push(result(vec![image.clone()]));
        session.messages.push(result(vec![image]));

        let images = loaded(&store, &session).await;
        let (Some(first), Some(second)) = (&images[0].data, &images[1].data) else {
            panic!("both read: {images:?}");
        };
        assert!(Arc::ptr_eq(first, second));
    }

    #[tokio::test]
    async fn images_before_the_last_compaction_are_not_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path().join("sessions"));
        let folder = store.images().join("parent");
        let mut session = session("look", 1);
        session
            .messages
            .push(result(vec![shown(folder.join("old.png"), b"old")]));
        session.messages.push(Message::Compaction {
            summary: "looked".into(),
            recent: String::new(),
        });
        session
            .messages
            .push(result(vec![shown(folder.join("new.png"), b"new")]));

        let images = loaded(&store, &session).await;
        assert_eq!(data(&images), [None, Some("bmV3")]);
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
    async fn deletes_a_saved_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path());
        let kept = session("keep", 1);
        let gone = session("drop", 2);
        store.save(&kept).await.expect("saves");
        store.save(&gone).await.expect("saves");

        store.delete(gone.id).await.expect("deletes");

        let listed = store.list().await.expect("lists");
        assert_eq!(listed.iter().map(|s| s.id).collect::<Vec<_>>(), [kept.id]);
        assert!(matches!(
            store.delete(gone.id).await,
            Err(Error::Io { source, .. }) if source.kind() == io::ErrorKind::NotFound
        ));
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
    async fn a_broken_curios_store_never_stops_a_save_or_load() {
        let repo = tempfile::tempdir().expect("tempdir");
        let status = std::process::Command::new("git")
            .args(["init", "--quiet"])
            .arg(repo.path())
            .status()
            .expect("runs git");
        assert!(status.success());
        let mut session = Session::new("glm-5.3", repo.path().into());
        session.messages.push(Message::User("hi".into()));
        session.curio_root = Some(repo.path().canonicalize().expect("exists"));
        session.curio_receipts.insert("batch".into());
        // An ignore exception that would put the session's state in git,
        // which the Curios store refuses to open with.
        let curios = repo.path().join(".nth/curios");
        std::fs::create_dir_all(&curios).expect("creates");
        std::fs::write(
            curios.join(".gitignore"),
            format!("*\n!{0}/\n!{0}/state.json\n", session.id),
        )
        .expect("writes");
        assert!(session.acknowledge_curios().is_err());
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path());

        store.save(&session).await.expect("saves");
        let loaded = store.load(session.id).await.expect("loads");

        assert_eq!(loaded.curio_receipts, session.curio_receipts);
    }

    #[tokio::test]
    async fn a_missing_directory_is_an_empty_store() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::at(dir.path().join("nope"));

        assert_eq!(store.list().await.expect("lists"), []);
    }

    #[test]
    fn ages_round_down_to_the_largest_unit() {
        let now = SystemTime::now();
        let ago = |secs| age(now, now - Duration::from_secs(secs));

        assert_eq!(ago(5), "now");
        assert_eq!(ago(5 * 60 + 59), "5m");
        assert_eq!(ago(3 * 3_600), "3h");
        assert_eq!(ago(2 * 86_400 + 1), "2d");
    }
}
