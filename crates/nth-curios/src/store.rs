use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::*;

const MAX_STATE: u64 = 16 * 1024 * 1024;
const MAX_BODY: usize = 16 * 1024;

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
    session: String,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default = "schema")]
    schema_version: u32,
    #[serde(default)]
    version: u64,
    #[serde(default)]
    curios: BTreeMap<String, Entry>,
    #[serde(default)]
    requests: BTreeMap<String, Receipt>,
    #[serde(default)]
    batches: Vec<Batch>,
    #[serde(default)]
    answered_message_ids: BTreeSet<String>,
    #[serde(default)]
    reviewed_revisions: BTreeMap<String, u64>,
    /// Curios adopted by another session, by id, to that session. One still
    /// in `curios` is mid-move and already gone from this store.
    #[serde(default)]
    moved: BTreeMap<String, String>,
}
fn schema() -> u32 {
    1
}
impl State {
    /// Refuses a curio another session adopted, even one still mid-move.
    fn here(&self, cid: &str) -> Result<()> {
        match self.moved.get(cid) {
            Some(to) => Err(Error::NotFound(format!("Curio moved to session {to}"))),
            None => Ok(()),
        }
    }
    fn entry(&self, cid: &str) -> Result<&Entry> {
        self.here(cid)?;
        self.curios.get(cid).ok_or_else(curio_not_found)
    }
}
fn curio_not_found() -> Error {
    Error::NotFound("Curio not found".into())
}
#[derive(Serialize, Deserialize)]
struct Entry {
    curio: Curio,
    revisions: Vec<Revision>,
    threads: Vec<Thread>,
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    fingerprint: String,
    result: serde_json::Value,
}

/// What a request's receipt keeps of its result, and the result a retry of
/// it gets back. A whole thread per reply would grow the state with the
/// square of the thread's length, so a thread is kept as it stood but with
/// only the message the request added, and a batch, which the state keeps
/// anyway, by its id.
trait Replay: Serialize + DeserializeOwned {
    fn receipt(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(self)?)
    }
    fn replay(_: &State, receipt: serde_json::Value) -> Result<Self> {
        Ok(serde_json::from_value(receipt)?)
    }
}

impl Replay for Published {}

#[derive(Serialize, Deserialize)]
struct ThreadReceipt {
    /// The thread as the request left it, holding only its last message.
    thread: Thread,
    messages: usize,
}

impl Replay for Thread {
    fn receipt(&self) -> Result<serde_json::Value> {
        let mut thread = self.clone();
        let messages = thread.messages.len();
        thread.messages.drain(..messages.saturating_sub(1));
        Ok(serde_json::to_value(ThreadReceipt { thread, messages })?)
    }
    fn replay(state: &State, receipt: serde_json::Value) -> Result<Self> {
        let ThreadReceipt {
            mut thread,
            messages,
        } = serde_json::from_value(receipt)?;
        // Messages are only ever appended, so the ones before the request's
        // own are those the thread holds now.
        let earlier = state
            .curios
            .values()
            .flat_map(|entry| &entry.threads)
            .find(|current| current.id == thread.id)
            .ok_or_else(|| Error::NotFound("Thread not found".into()))?
            .messages
            .iter()
            .take(messages.saturating_sub(thread.messages.len()))
            .cloned();
        thread.messages.splice(..0, earlier);
        Ok(thread)
    }
}

impl Replay for Batch {
    fn receipt(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({ "batch": self.id }))
    }
    fn replay(state: &State, receipt: serde_json::Value) -> Result<Self> {
        let id = receipt["batch"].as_str();
        state
            .batches
            .iter()
            .find(|batch| Some(batch.id.as_str()) == id)
            .cloned()
            .ok_or_else(|| Error::NotFound("Batch not found".into()))
    }
}

pub fn repository_root(cwd: &Path) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !output.status.success() {
        return Err(Error::RepoRequired);
    }
    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}
fn component(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 160
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(Error::Invalid("Invalid identifier".into()));
    }
    Ok(())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn text(body: &str) -> Result<()> {
    if body.trim().is_empty() || body.len() > MAX_BODY {
        return Err(Error::Invalid("Message must contain 1–16384 bytes".into()));
    }
    Ok(())
}
/// Refuses a path that goes through a symlink inside `.nth`, the part
/// Curios makes. What lies above the repository is the system's own: on
/// macOS the temporary folders sit under `/var`, a symlink to `/private/var`.
fn no_symlinks(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        if fs::symlink_metadata(ancestor).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(Error::Invalid(
                "Curios paths must not contain symlinks".into(),
            ));
        }
        if ancestor.file_name().is_some_and(|name| name == ".nth") {
            break;
        }
    }
    Ok(())
}
fn atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().expect("state parent");
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn verify_ignored(repo: &Path, path: &Path) -> Result<()> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["check-ignore", "--verbose", "-z", "--no-index", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    {
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin.write_all(path.as_os_str().as_encoded_bytes())?;
        stdin.write_all(&[0])?;
    }
    let output = child.wait_with_output()?;
    // check-ignore can report a matching negation with a successful status.
    let pattern = output.stdout.split(|b| *b == 0).nth(2);
    if !output.status.success()
        || pattern.is_none_or(|pattern| pattern.is_empty() || pattern.starts_with(b"!"))
    {
        return Err(Error::Conflict(
            "Existing ignore rules expose Curios content".into(),
        ));
    }
    Ok(())
}

fn verify_nested_ignores(repo: &Path, path: &Path, root: bool) -> Result<()> {
    for item in fs::read_dir(path)? {
        let item = item?;
        let kind = item.file_type()?;
        if kind.is_symlink() {
            return Err(Error::Conflict("Symlink inside Curios storage".into()));
        }
        if kind.is_dir() {
            verify_nested_ignores(repo, &item.path(), false)?;
        } else if !root && item.file_name() == ".gitignore" {
            return Err(Error::Conflict(
                "Nested ignore rules inside Curios storage require removal".into(),
            ));
        }
        verify_ignored(repo, &item.path())?;
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Fails the n-th save from now; 0 fails none.
    pub(crate) static FAIL_STATE_COMMIT: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

impl Store {
    /// `repo_root` is captured at session preparation; retain it across managed worktree moves.
    pub fn new(repo_root: impl AsRef<Path>, session_id: &str) -> Result<Self> {
        component(session_id)?;
        let root = repo_root.as_ref().canonicalize()?;
        if repository_root(&root)? != root {
            return Err(Error::RepoRequired);
        }
        let base = root.join(".nth/curios");
        no_symlinks(&base)?;
        fs::create_dir_all(&base)?;
        let ignore = base.join(".gitignore");
        if !ignore.exists() {
            atomic(&ignore, b"*\n")?;
        }
        verify_nested_ignores(&root, &base, true)?;
        // Require the actual owner directory to be ignored too, protecting future
        // curio/revisions directories and arbitrary source filenames.
        let owner = base.join(session_id);
        no_symlinks(&owner)?;
        no_symlinks(&owner.join("source"))?;
        fs::create_dir_all(owner.join("source"))?;
        for probe in [
            owner.clone(),
            owner.join("source"),
            owner.join("state.json"),
            owner.join("lock"),
            owner.join("delivery.lock"),
        ] {
            verify_ignored(&root, &probe)?;
        }
        let store = Self {
            root,
            session: session_id.into(),
        };
        no_symlinks(&store.path())?;
        fs::create_dir_all(store.source_dir())?;
        {
            let _lock = store.lock()?;
            let state = store.load()?;
            store.recover(&state)?;
        }
        store.prepare_template()?;
        Ok(store)
    }
    pub fn repo_root(&self) -> &Path {
        &self.root
    }
    pub fn session_id(&self) -> &str {
        &self.session
    }
    pub fn path(&self) -> PathBuf {
        self.root.join(".nth/curios").join(&self.session)
    }
    pub fn source_dir(&self) -> PathBuf {
        self.path().join("source")
    }
    /// Where an adopted curio's HTML was copied to edit; see [`Self::adopt`].
    pub fn adopted_source(&self, curio_id: &str) -> PathBuf {
        self.source_dir()
            .join("adopted")
            .join(format!("{curio_id}.html"))
    }
    pub fn prepare_template(&self) -> Result<PathBuf> {
        let dir = self.source_dir().join("templates");
        no_symlinks(&dir)?;
        fs::create_dir_all(&dir)?;
        let path = dir.join("quiet-folio.html");
        no_symlinks(&path)?;
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(include_bytes!("../assets/templates/quiet-folio.html"))?;
                file.sync_all()?;
                File::open(&dir)?.sync_all()?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        Ok(path)
    }
    fn lock(&self) -> Result<File> {
        no_symlinks(&self.path())?;
        no_symlinks(&self.path().join("lock"))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.path().join("lock"))?;
        file.lock_exclusive()?;
        Ok(file)
    }
    fn load(&self) -> Result<State> {
        let path = self.path().join("state.json");
        no_symlinks(&path)?;
        if !path.exists() {
            return Ok(State {
                schema_version: 1,
                ..State::default()
            });
        }
        let file = File::open(path)?;
        if file.metadata()?.len() > MAX_STATE {
            return Err(Error::Invalid("State exceeds limit".into()));
        }
        let state: State = serde_json::from_reader(file)?;
        if state.schema_version != 1 {
            return Err(Error::Invalid("Unsupported state schema".into()));
        }
        Ok(state)
    }
    fn recover(&self, state: &State) -> Result<()> {
        for item in fs::read_dir(self.path())? {
            let item = item?;
            let cid = item.file_name().to_string_lossy().into_owned();
            if !item.file_type()?.is_dir() || uuid::Uuid::parse_str(&cid).is_err() {
                continue;
            }
            let Some(entry) = state.curios.get(&cid) else {
                fs::remove_dir_all(item.path())?;
                continue;
            };
            let revisions = item.path().join("revisions");
            no_symlinks(&revisions)?;
            for revision in fs::read_dir(&revisions)? {
                let revision = revision?;
                let name = revision.file_name().to_string_lossy().into_owned();
                if (name.starts_with(".tmp")
                    || name
                        .parse::<u64>()
                        .is_ok_and(|n| !entry.revisions.iter().any(|r| r.number == n)))
                    && revision.file_type()?.is_dir()
                {
                    fs::remove_dir_all(revision.path())?;
                }
            }
        }
        Ok(())
    }
    fn save(&self, state: &mut State) -> Result<()> {
        state.version += 1;
        let bytes = serde_json::to_vec(state)?;
        if bytes.len() as u64 > MAX_STATE {
            return Err(Error::Invalid("State exceeds limit".into()));
        }
        #[cfg(test)]
        if FAIL_STATE_COMMIT.with(|fail| {
            let n = fail.get();
            fail.set(n.saturating_sub(1));
            n == 1
        }) {
            return Err(Error::Io(std::io::Error::other(
                "Injected pre-commit failure",
            )));
        }
        atomic(&self.path().join("state.json"), &bytes)
    }
    fn mutate<T: Replay>(
        &self,
        request: &str,
        input: impl Serialize,
        change: impl FnOnce(&mut State) -> Result<T>,
    ) -> Result<T> {
        component(request)?;
        let fingerprint = hash(&serde_json::to_vec(&input)?);
        let _lock = self.lock()?;
        let mut state = self.load()?;
        if let Some(receipt) = state.requests.get(request) {
            if receipt.fingerprint != fingerprint {
                return Err(Error::Conflict(
                    "Request ID reused with different input".into(),
                ));
            }
            return T::replay(&state, receipt.result.clone());
        }
        if state.requests.len() >= 20000 {
            return Err(Error::Invalid("Session request limit reached".into()));
        }
        let result = change(&mut state)?;
        state.requests.insert(
            request.into(),
            Receipt {
                fingerprint,
                result: result.receipt()?,
            },
        );
        self.save(&mut state)?;
        Ok(result)
    }
    pub fn list(&self) -> Result<Vec<Curio>> {
        let _lock = self.lock()?;
        let state = self.load()?;
        Ok(state
            .curios
            .into_values()
            .filter(|e| !state.moved.contains_key(&e.curio.id))
            .map(|e| e.curio)
            .collect())
    }
    pub fn state_version(&self) -> Result<u64> {
        let _lock = self.lock()?;
        Ok(self.load()?.version)
    }
    pub fn snapshot(&self, curio_id: &str, revision: Option<u64>) -> Result<Snapshot> {
        let _lock = self.lock()?;
        let state = self.load()?;
        let entry = state.entry(curio_id)?;
        let number = revision.unwrap_or(entry.curio.current_revision);
        let rev = entry
            .revisions
            .iter()
            .find(|r| r.number == number)
            .ok_or_else(|| Error::NotFound("Revision not found".into()))?
            .clone();
        let mut threads = entry.threads.clone();
        for thread in &mut threads {
            let original = entry
                .revisions
                .iter()
                .find(|r| r.number == thread.created_revision);
            let attached = if thread.created_revision == number {
                true
            } else if let Some(target) = &thread.target.element_id {
                let old = original.and_then(|r| r.anchors.iter().find(|a| &a.id == target));
                let new = rev.anchors.iter().find(|a| &a.id == target);
                old.zip(new).is_some_and(|(a, b)| a.label == b.label)
            } else {
                false
            };
            thread.attachment = if attached { "attached" } else { "detached" }.into();
        }
        Ok(Snapshot {
            curio: entry.curio.clone(),
            revision: rev,
            threads,
            revisions: entry.revisions.clone(),
            state_version: state.version,
        })
    }
    pub fn publish_file(&self, source: &Path, mut publish: Publish) -> Result<Published> {
        no_symlinks(source)?;
        let source = source.canonicalize()?;
        if !source.starts_with(self.source_dir().canonicalize()?) {
            return Err(Error::Invalid(
                "Source must be inside this session's workspace".into(),
            ));
        }
        let file = File::open(source)?;
        let mut html = String::new();
        file.take(MAX_HTML_BYTES as u64 + 1)
            .read_to_string(&mut html)?;
        publish.html = html;
        self.publish(publish)
    }
    pub fn publish(&self, publish: Publish) -> Result<Published> {
        let anchors = validate(&publish.html)?;
        if publish
            .title
            .as_ref()
            .is_some_and(|s| s.len() > 240 || s.trim().is_empty())
        {
            return Err(Error::Invalid("Invalid title".into()));
        }
        self.mutate(&publish.request_id, ("publish", &publish), |state| {
            let cid = publish.curio_id.clone().unwrap_or_else(id);
            component(&cid)?;
            state.here(&cid)?;
            let current = state.curios.get(&cid).map(|e| e.curio.current_revision);
            if current != publish.expected_revision {
                return Err(Error::Conflict(
                    "Expected current revision does not match".into(),
                ));
            }
            if current.is_none() && state.curios.len() >= 100 {
                return Err(Error::Invalid("Curio limit reached".into()));
            }
            let number = current.unwrap_or(0) + 1;
            if number > 1000 {
                return Err(Error::Invalid("Revision limit reached".into()));
            }
            let title = publish
                .title
                .clone()
                .or_else(|| state.curios.get(&cid).map(|e| e.curio.title.clone()))
                .unwrap_or_else(|| "Untitled".into());
            let revision = Revision {
                number,
                title: title.clone(),
                hash: hash(publish.html.as_bytes()),
                anchors,
                html_url: format!("/content/{number}"),
            };
            let dir = self.path().join(&cid).join("revisions");
            no_symlinks(&dir)?;
            fs::create_dir_all(&dir)?;
            let destination = dir.join(number.to_string());
            // A directory not referenced by committed state is an interrupted publication.
            if destination.exists() {
                fs::remove_dir_all(&destination)?;
            }
            let staging = tempfile::tempdir_in(&dir)?;
            atomic(&staging.path().join("index.html"), publish.html.as_bytes())?;
            atomic(
                &staging.path().join("revision.json"),
                &serde_json::to_vec(&revision)?,
            )?;
            fs::rename(staging.path(), &destination)?;
            File::open(&dir)?.sync_all()?;
            let entry = state.curios.entry(cid.clone()).or_insert_with(|| Entry {
                curio: Curio {
                    id: cid,
                    title: title.clone(),
                    session_id: self.session.clone(),
                    current_revision: 0,
                    adopted_from: None,
                },
                revisions: vec![],
                threads: vec![],
            });
            entry.curio.title = title;
            entry.curio.current_revision = number;
            let detached_thread_ids = entry
                .threads
                .iter()
                .filter(|t| {
                    !t.target.element_id.as_ref().is_some_and(|target| {
                        revision.anchors.iter().any(|a| {
                            &a.id == target
                                && entry
                                    .revisions
                                    .iter()
                                    .find(|r| r.number == t.created_revision)
                                    .is_some_and(|r| r.anchors.contains(a))
                        })
                    })
                })
                .map(|t| t.id.clone())
                .collect();
            entry.revisions.push(revision.clone());
            Ok(Published {
                curio: entry.curio.clone(),
                revision,
                detached_thread_ids,
            })
        })
    }
    pub fn content(&self, curio_id: &str, revision: u64) -> Result<String> {
        component(curio_id)?;
        self.snapshot(curio_id, Some(revision))?;
        let path = self
            .path()
            .join(curio_id)
            .join("revisions")
            .join(revision.to_string())
            .join("index.html");
        no_symlinks(&path)?;
        let mut html = String::new();
        File::open(path)?
            .take(MAX_HTML_BYTES as u64 + 1)
            .read_to_string(&mut html)?;
        if html.len() > MAX_HTML_BYTES {
            return Err(Error::Invalid("HTML exceeds limit".into()));
        }
        Ok(html)
    }
    pub fn comment(&self, cid: &str, comment: Comment) -> Result<Thread> {
        text(&comment.body)?;
        if comment.target.label.len() > 512 || comment.target.quote.len() > 2048 {
            return Err(Error::Invalid("Target context exceeds limit".into()));
        }
        self.mutate(&comment.request_id, ("comment", cid, &comment), |state| {
            state.here(cid)?;
            let entry = state.curios.get_mut(cid).ok_or_else(curio_not_found)?;
            let rev = entry
                .revisions
                .iter()
                .find(|r| r.number == comment.revision)
                .ok_or_else(|| Error::NotFound("Revision not found".into()))?;
            if entry.threads.len() >= 1000 {
                return Err(Error::Invalid("Thread limit reached".into()));
            }
            if let Some(target) = &comment.target.element_id
                && !rev.anchors.iter().any(|a| &a.id == target)
            {
                return Err(Error::Invalid("Unknown target ID".into()));
            }
            if comment.target.kind == TargetKind::Element && comment.target.element_id.is_none() {
                return Err(Error::Invalid("Element target requires ID".into()));
            }
            if comment.target.kind == TargetKind::Region && comment.target.rect.is_none() {
                return Err(Error::Invalid("Region requires rectangle".into()));
            }
            if let Some(r) = &comment.target.rect
                && (![r.x, r.y, r.width, r.height]
                    .iter()
                    .all(|v| v.is_finite() && *v >= 0.0 && *v <= 1.0 + 1e-9)
                    || r.width == 0.0
                    || r.height == 0.0
                    || r.x + r.width > 1.0 + 1e-9
                    || r.y + r.height > 1.0 + 1e-9)
            {
                return Err(Error::Invalid("Invalid rectangle".into()));
            }
            let thread = Thread {
                id: id(),
                number: entry.threads.len() as u64 + 1,
                version: 1,
                created_revision: comment.revision,
                target: comment.target.clone(),
                attachment: "attached".into(),
                status: Status::Open,
                messages: vec![Message {
                    id: id(),
                    author: Author::Human,
                    body: comment.body.clone(),
                    created_at: now(),
                    delivery: Delivery::Saved,
                }],
            };
            state
                .reviewed_revisions
                .insert(thread.messages[0].id.clone(), comment.revision);
            entry.threads.push(thread.clone());
            Ok(thread)
        })
    }
    pub fn reply(&self, cid: &str, tid: &str, reply: Reply, author: Author) -> Result<Thread> {
        text(&reply.body)?;
        if author == Author::Human && reply.mark_addressed {
            return Err(Error::Invalid("Human reply cannot mark addressed".into()));
        }
        self.mutate(
            &reply.request_id,
            ("reply", cid, tid, &reply, &author),
            |state| {
                state.here(cid)?;
                let entry = state.curios.get_mut(cid).ok_or_else(curio_not_found)?;
                if reply
                    .revision
                    .is_some_and(|n| !entry.revisions.iter().any(|r| r.number == n))
                {
                    return Err(Error::NotFound("Revision not found".into()));
                }
                let thread = entry
                    .threads
                    .iter_mut()
                    .find(|t| t.id == tid)
                    .ok_or_else(|| Error::NotFound("Thread not found".into()))?;
                if thread.version != reply.expected_version {
                    return Err(Error::Conflict("Thread version changed".into()));
                }
                if thread.messages.len() >= 200 {
                    return Err(Error::Invalid("Message limit reached".into()));
                }
                if author == Author::Agent {
                    for m in &mut thread.messages {
                        if m.author == Author::Human
                            && matches!(m.delivery, Delivery::Queued | Delivery::Recorded)
                        {
                            state.answered_message_ids.insert(m.id.clone());
                            if m.delivery == Delivery::Recorded {
                                m.delivery = Delivery::Answered;
                            }
                        }
                    }
                    if reply.mark_addressed && thread.status != Status::Resolved {
                        thread.status = Status::Addressed;
                    }
                }
                thread.messages.push(Message {
                    id: id(),
                    author: author.clone(),
                    body: reply.body.clone(),
                    created_at: now(),
                    delivery: if author == Author::Human {
                        Delivery::Saved
                    } else {
                        Delivery::Answered
                    },
                });
                if author == Author::Human
                    && let Some(revision) = reply.revision
                {
                    state.reviewed_revisions.insert(
                        thread.messages.last().expect("new reply").id.clone(),
                        revision,
                    );
                }
                thread.version += 1;
                Ok(thread.clone())
            },
        )
    }
    pub fn set_status(
        &self,
        cid: &str,
        tid: &str,
        expected_version: u64,
        status: Status,
    ) -> Result<Thread> {
        if status == Status::Addressed {
            return Err(Error::Invalid("Use agent reply to mark addressed".into()));
        }
        let _lock = self.lock()?;
        let mut state = self.load()?;
        state.here(cid)?;
        let thread = state
            .curios
            .get_mut(cid)
            .and_then(|e| e.threads.iter_mut().find(|t| t.id == tid))
            .ok_or_else(|| Error::NotFound("Thread not found".into()))?;
        if thread.version != expected_version {
            return Err(Error::Conflict("Thread version changed".into()));
        }
        thread.status = status;
        thread.version += 1;
        let result = thread.clone();
        self.save(&mut state)?;
        Ok(result)
    }
    pub fn send(&self, cid: &str, send: Send) -> Result<Batch> {
        if send.message_ids.is_empty() || send.message_ids.len() > 200 {
            return Err(Error::Invalid("Send requires 1–200 messages".into()));
        }
        self.mutate(&send.request_id, ("send", cid, &send), |state| {
            state.here(cid)?;
            let entry = state.curios.get_mut(cid).ok_or_else(curio_not_found)?;
            let unique: std::collections::HashSet<_> = send.message_ids.iter().collect();
            if unique.len() != send.message_ids.len() {
                return Err(Error::Invalid("Duplicate message IDs".into()));
            }
            let mut messages = Vec::new();
            for thread in &mut entry.threads {
                let mut changed = false;
                for message in &mut thread.messages {
                    if unique.contains(&message.id) {
                        if message.author != Author::Human || message.delivery != Delivery::Saved {
                            return Err(Error::Conflict(
                                "Message is already sent or not human-authored".into(),
                            ));
                        }
                        message.delivery = Delivery::Queued;
                        changed = true;
                        messages.push(SubmittedMessage {
                            thread_id: thread.id.clone(),
                            thread_version: thread.version + 1,
                            message: message.clone(),
                            target: thread.target.clone(),
                            reviewed_revision: state.reviewed_revisions.get(&message.id).copied(),
                        });
                    }
                }
                if changed {
                    thread.version += 1;
                }
            }
            if messages.len() != unique.len() {
                return Err(Error::NotFound("Message not found".into()));
            }
            let batch = Batch {
                id: id(),
                session_id: self.session.clone(),
                curio_id: cid.into(),
                revision: entry.curio.current_revision,
                messages,
                recorded: false,
            };
            state.batches.push(batch.clone());
            Ok(batch)
        })
    }
    pub fn batches(&self, pending_only: bool) -> Result<Vec<Batch>> {
        let _lock = self.lock()?;
        Ok(self
            .load()?
            .batches
            .into_iter()
            .filter(|b| !pending_only || !b.recorded)
            .collect())
    }
    /// Call only after the owning session, including its receipt IDs, was saved.
    /// Delivery is not content, so threads keep their version: a reply the
    /// model writes after this still matches the version its notice carried.
    pub fn acknowledge_recorded(&self, batch_ids: &[String]) -> Result<()> {
        let _lock = self.lock()?;
        let mut state = self.load()?;
        for batch in &mut state.batches {
            if batch_ids.contains(&batch.id) {
                batch.recorded = true;
                if let Some(entry) = state.curios.get_mut(&batch.curio_id) {
                    for message in entry.threads.iter_mut().flat_map(|t| &mut t.messages) {
                        if batch.messages.iter().any(|m| m.message.id == message.id)
                            && message.delivery == Delivery::Queued
                        {
                            message.delivery = if state.answered_message_ids.contains(&message.id) {
                                Delivery::Answered
                            } else {
                                Delivery::Recorded
                            };
                        }
                    }
                }
            }
        }
        self.save(&mut state)
    }
    /// Holds a process-wide advisory lease until dropped. A second delivery owner fails.
    pub fn claim_delivery(&self) -> Result<File> {
        no_symlinks(&self.path().join("delivery.lock"))?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.path().join("delivery.lock"))?;
        file.try_lock_exclusive()
            .map_err(|_| Error::Conflict("Session already has a delivery owner".into()))?;
        Ok(file)
    }
    /// Moves `curio_id` from session `from` into this one, with its
    /// revisions, threads and batches, so its feedback reaches this session.
    /// Refused while `from` is running, since it would take its feedback.
    ///
    /// The old store marks the curio moved first, which hides it there, then
    /// this one takes it, then the old one lets it go: a move cut short at
    /// any point is finished by adopting again.
    pub fn adopt(&self, from: &str, curio_id: &str) -> Result<Adopted> {
        component(from)?;
        component(curio_id)?;
        if from == self.session {
            return Err(Error::Invalid("Curio is already this session's".into()));
        }
        let old = Self {
            root: self.root.clone(),
            session: from.into(),
        };
        no_symlinks(&old.path())?;
        if !old.path().join("state.json").exists() {
            return Err(curio_not_found());
        }
        let _owner = old
            .claim_delivery()
            .map_err(|_| Error::Conflict("Curio is owned by a running session".into()))?;
        // Both locks, always in the same order, so two adoptions never wait
        // on each other.
        let _locks = if old.session < self.session {
            (old.lock()?, self.lock()?)
        } else {
            (self.lock()?, old.lock()?)
        };
        let mut from_state = old.load()?;
        match from_state.moved.get(curio_id) {
            Some(to) if *to != self.session => from_state.here(curio_id)?,
            Some(_) => {}
            None => {
                from_state.entry(curio_id)?;
                from_state
                    .moved
                    .insert(curio_id.into(), self.session.clone());
                old.save(&mut from_state)?;
            }
        }
        let mut state = self.load()?;
        if !state.curios.contains_key(curio_id) {
            let entry = from_state
                .curios
                .get(curio_id)
                .ok_or_else(curio_not_found)?;
            if state.curios.len() >= 100 {
                return Err(Error::Invalid("Curio limit reached".into()));
            }
            let dir = self.path().join(curio_id);
            no_symlinks(&dir)?;
            // What an earlier attempt copied before it was cut short.
            if dir.exists() {
                fs::remove_dir_all(&dir)?;
            }
            let staging = tempfile::tempdir_in(self.path())?;
            for revision in &entry.revisions {
                let number = revision.number.to_string();
                let source = old.path().join(curio_id).join("revisions").join(&number);
                no_symlinks(&source)?;
                let target = staging.path().join("revisions").join(&number);
                fs::create_dir_all(&target)?;
                for file in ["index.html", "revision.json"] {
                    atomic(&target.join(file), &fs::read(source.join(file))?)?;
                }
            }
            fs::rename(staging.path(), &dir)?;
            File::open(self.path())?.sync_all()?;
            let mut curio = entry.curio.clone();
            curio.session_id = self.session.clone();
            curio.adopted_from = Some(from.into());
            let messages: BTreeSet<_> = entry
                .threads
                .iter()
                .flat_map(|t| &t.messages)
                .map(|m| m.id.clone())
                .collect();
            for id in &messages {
                if let Some(&revision) = from_state.reviewed_revisions.get(id) {
                    state.reviewed_revisions.insert(id.clone(), revision);
                }
                if from_state.answered_message_ids.contains(id) {
                    state.answered_message_ids.insert(id.clone());
                }
            }
            state.batches.extend(
                from_state
                    .batches
                    .iter()
                    .filter(|b| b.curio_id == curio_id)
                    .map(|b| Batch {
                        session_id: self.session.clone(),
                        ..b.clone()
                    }),
            );
            state.curios.insert(
                curio_id.into(),
                Entry {
                    curio,
                    revisions: entry.revisions.clone(),
                    threads: entry.threads.clone(),
                },
            );
            self.save(&mut state)?;
        }
        if from_state.curios.remove(curio_id).is_some() {
            from_state.batches.retain(|b| b.curio_id != curio_id);
            old.save(&mut from_state)?;
        }
        let gone = old.path().join(curio_id);
        if gone.exists() {
            fs::remove_dir_all(gone)?;
        }
        let entry = &state.curios[curio_id];
        let source = self.adopted_source(curio_id);
        no_symlinks(&source)?;
        // The model may have edited it since an earlier adoption.
        if !source.exists() {
            fs::create_dir_all(source.parent().expect("adopted folder"))?;
            let html = fs::read(
                self.path()
                    .join(curio_id)
                    .join("revisions")
                    .join(entry.curio.current_revision.to_string())
                    .join("index.html"),
            )?;
            atomic(&source, &html)?;
        }
        Ok(Adopted {
            curio: entry.curio.clone(),
            source,
        })
    }
}

pub fn discover(repo_root: &Path) -> Result<Vec<Curio>> {
    let base = repo_root.join(".nth/curios");
    if !base.exists() {
        return Ok(vec![]);
    }
    no_symlinks(&base)?;
    let mut result = Vec::new();
    for item in fs::read_dir(base)?.take(1000) {
        let item = item?;
        if item.file_type()?.is_dir() {
            let session = item.file_name().to_string_lossy().into_owned();
            component(&session)?;
            let store = Store {
                root: repo_root.to_owned(),
                session,
            };
            result.extend(store.list()?);
        }
    }
    Ok(result)
}
