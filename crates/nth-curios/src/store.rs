use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use fs2::FileExt;
use nth_directories::Directories;
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
impl Entry {
    fn attached(&self, thread: &Thread, revision: &Revision) -> bool {
        if thread.created_revision == revision.number {
            return true;
        }
        thread.target.element_id.as_ref().is_some_and(|id| {
            self.revisions
                .iter()
                .find(|r| r.number == thread.created_revision)
                .is_some_and(|r| r.anchors.iter().any(|anchor| &anchor.id == id))
                && revision.anchors.iter().any(|anchor| &anchor.id == id)
        })
    }
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

/// The main checkout for `cwd`, after validating the repository: a linked
/// worktree's curios live with its main checkout, so every worktree of a
/// session finds the same store. Canonical, as [`Store::repo_root`] is.
pub fn owner_root(cwd: &Path) -> Result<PathBuf> {
    let root = repository_root(cwd)?;
    let main = nth_directories::main_project_root(&root).unwrap_or(root);
    Ok(main.canonicalize()?)
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
        if ancestor
            .file_name()
            .is_some_and(|name| name == nth_directories::NTH)
        {
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

fn exposed() -> Error {
    Error::Conflict("Existing ignore rules expose Curios content".into())
}

/// Asks one `git check-ignore` about every path: a process per path made
/// building a store cost as much as the whole tree had ever published.
fn verify_ignored(repo: &Path, paths: &[PathBuf]) -> Result<()> {
    if paths.is_empty() {
        return Ok(());
    }
    #[cfg(test)]
    checked(repo, paths.len());
    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "check-ignore",
            "--verbose",
            "--non-matching",
            "-z",
            "--no-index",
            "--stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Left inherited, git's complaints would draw over the TUI.
        .stderr(Stdio::piped())
        .spawn()?;
    let mut stdin = child.stdin.take().expect("piped stdin");
    // git answers as it reads, so a long list written before reading would
    // fill both pipes and wait forever.
    let (written, output) = std::thread::scope(|scope| {
        let writer = scope.spawn(move || -> std::io::Result<()> {
            for path in paths {
                stdin.write_all(path.as_os_str().as_encoded_bytes())?;
                stdin.write_all(&[0])?;
            }
            Ok(())
        });
        let output = child.wait_with_output();
        let written = writer
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("check-ignore input failed")));
        (written, output)
    });
    let output = output?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            return Err(exposed());
        }
        return Err(Error::Io(std::io::Error::other(format!(
            "git check-ignore failed: {stderr}"
        ))));
    }
    written?;
    // A record is source, line, pattern and path; a path no rule matches has
    // the first three empty. Pairing each record with the path sent fails
    // the batch on one out of place instead of crediting it another's rule.
    let mut fields = output.stdout.split(|b| *b == 0);
    for path in paths {
        let (Some(_), Some(_), Some(pattern), Some(echoed)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(exposed());
        };
        // check-ignore reports a matching negation as a match.
        if echoed != path.as_os_str().as_encoded_bytes()
            || pattern.is_empty()
            || pattern.starts_with(b"!")
        {
            return Err(exposed());
        }
    }
    Ok(())
}

/// Gathers every path below `path` for one [`verify_ignored`], refusing
/// what would let a file slip past the base rules.
fn collect_nested(path: &Path, root: bool, into: &mut Vec<PathBuf>) -> Result<()> {
    for item in fs::read_dir(path)? {
        let item = item?;
        let kind = item.file_type()?;
        if kind.is_symlink() {
            return Err(Error::Conflict("Symlink inside Curios storage".into()));
        }
        if kind.is_dir() {
            collect_nested(&item.path(), false, into)?;
        } else if !root && item.file_name() == ".gitignore" {
            return Err(Error::Conflict(
                "Nested ignore rules inside Curios storage require removal".into(),
            ));
        }
        into.push(item.path());
    }
    Ok(())
}

/// The roots whose whole `.nth/curios` tree passed the walk in this process,
/// with what their base `.gitignore` looked like then. The tree holds every
/// session's every revision, so walking it on each [`Store::new`] made the
/// cost grow with use; only a changed base rule walks it again.
static VERIFIED: Mutex<BTreeMap<PathBuf, Stamp>> = Mutex::new(BTreeMap::new());

#[cfg(test)]
pub(crate) fn forget_verified(root: &Path) {
    VERIFIED
        .lock()
        .expect("verified lock poisoned")
        .remove(root);
}

/// How many paths each `check-ignore` under a root asked about. Keyed by
/// root, so tests running in parallel each see only their own.
#[cfg(test)]
static CHECKS: Mutex<BTreeMap<PathBuf, Vec<usize>>> = Mutex::new(BTreeMap::new());

#[cfg(test)]
fn checked(root: &Path, paths: usize) {
    CHECKS
        .lock()
        .expect("checks lock poisoned")
        .entry(root.to_owned())
        .or_default()
        .push(paths);
}

#[cfg(test)]
pub(crate) fn take_checks(root: &Path) -> Vec<usize> {
    CHECKS
        .lock()
        .expect("checks lock poisoned")
        .remove(root)
        .unwrap_or_default()
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
        let base = Directories::from_root(&root).curios();
        no_symlinks(&base)?;
        fs::create_dir_all(&base)?;
        let ignore = base.join(".gitignore");
        if !ignore.exists() {
            atomic(&ignore, b"*\n")?;
        }
        let stamp = Stamp::of(&ignore);
        let walked = stamp.is_some()
            && VERIFIED.lock().expect("verified lock poisoned").get(&root) == stamp.as_ref();
        let mut paths = Vec::new();
        if !walked {
            collect_nested(&base, true, &mut paths)?;
        }
        // Require the actual owner directory to be ignored too, protecting future
        // curio/revisions directories and arbitrary source filenames.
        let owner = base.join(session_id);
        no_symlinks(&owner)?;
        no_symlinks(&owner.join("source"))?;
        fs::create_dir_all(owner.join("source"))?;
        if walked {
            // This session's own folder is walked every time, even when the
            // tree is trusted: the model writes its source workspace, and its
            // cost grows with this session alone.
            collect_nested(&owner, false, &mut paths)?;
        }
        paths.extend([
            owner.clone(),
            owner.join("source"),
            owner.join("state.json"),
            owner.join("lock"),
            owner.join("delivery.lock"),
        ]);
        paths.sort();
        paths.dedup();
        verify_ignored(&root, &paths)?;
        if !walked && let Some(stamp) = stamp {
            VERIFIED
                .lock()
                .expect("verified lock poisoned")
                .insert(root.clone(), stamp);
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
    /// `session`'s store in the main checkout of `state_root`, its template
    /// prepared. Spawns git and may wait on another process's lock, so an
    /// async caller runs it under [`offload`].
    pub fn open(state_root: &Path, session: &str) -> Result<Self> {
        Self::new(owner_root(state_root)?, session)
    }
    pub fn repo_root(&self) -> &Path {
        &self.root
    }
    pub fn session_id(&self) -> &str {
        &self.session
    }
    pub fn path(&self) -> PathBuf {
        Directories::from_root(&self.root).curio_owner(&self.session)
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
    /// Where [`Self::prepare_template`] writes the template.
    pub fn template_path(&self) -> PathBuf {
        self.source_dir().join("templates").join("quiet-folio.html")
    }
    pub fn prepare_template(&self) -> Result<PathBuf> {
        let path = self.template_path();
        let dir = self.source_dir().join("templates");
        no_symlinks(&dir)?;
        fs::create_dir_all(&dir)?;
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
    fn lock_file(&self) -> Result<File> {
        no_symlinks(&self.path())?;
        no_symlinks(&self.path().join("lock"))?;
        Ok(OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.path().join("lock"))?)
    }
    pub(crate) fn lock(&self) -> Result<File> {
        let file = self.lock_file()?;
        file.lock_exclusive()?;
        Ok(file)
    }
    /// [`Self::lock`] for one who would rather come back later: `None` while
    /// another holds it.
    fn try_lock(&self) -> Result<Option<File>> {
        let file = self.lock_file()?;
        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(file)),
            Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
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
        self.listing().map(|(curios, _)| curios)
    }
    /// [`Self::list`] with the ids of the curios that have feedback queued,
    /// read under one lock.
    fn listing(&self) -> Result<(Vec<Curio>, BTreeSet<String>)> {
        let _lock = self.lock()?;
        let state = self.load()?;
        let queued = state
            .batches
            .iter()
            .filter(|b| !b.recorded)
            .map(|b| b.curio_id.clone())
            .collect();
        let curios = state
            .curios
            .into_values()
            .filter(|e| !state.moved.contains_key(&e.curio.id))
            .map(|e| {
                let mut curio = e.curio;
                curio.updated_at = self.updated_at(&curio);
                curio
            })
            .collect();
        Ok((curios, queued))
    }
    /// When the current revision was written: publishing never rewrites a
    /// revision, so its file's time is the publish time, except for an
    /// adopted curio, whose `index.html` adoption rewrites, so it is the time
    /// it was adopted.
    fn updated_at(&self, curio: &Curio) -> Option<u64> {
        let path = Directories::from_root(&self.root)
            .curio_revision(&self.session, &curio.id, curio.current_revision)
            .join("index.html");
        let modified = fs::metadata(path).and_then(|m| m.modified()).ok()?;
        Some(modified.duration_since(UNIX_EPOCH).ok()?.as_secs())
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
            let attached = entry.attached(thread, &rev);
            thread.attachment = if attached { "attached" } else { "detached" }.into();
        }
        Ok(Snapshot {
            curio: entry.curio.clone(),
            revision: rev,
            threads,
            revisions: entry.revisions.clone(),
            state_version: state.version,
            listening: self.listening(),
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
                    updated_at: None,
                },
                revisions: vec![],
                threads: vec![],
            });
            entry.curio.title = title;
            entry.curio.current_revision = number;
            let detached_thread_ids = entry
                .threads
                .iter()
                .filter(|t| !entry.attached(t, &revision))
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
        let path = Directories::from_root(&self.root)
            .curio_revision(&self.session, curio_id, revision)
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
        self.load_batches(pending_only)
    }
    /// [`Self::batches`] for a poll: `None` at once while another holds the
    /// lock, rather than waiting out its write.
    pub fn try_batches(&self, pending_only: bool) -> Result<Option<Vec<Batch>>> {
        let Some(_lock) = self.try_lock()? else {
            return Ok(None);
        };
        self.load_batches(pending_only).map(Some)
    }
    fn load_batches(&self, pending_only: bool) -> Result<Vec<Batch>> {
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
    /// Whether a running session holds [`Self::claim_delivery`]'s lease, in
    /// this process or another.
    pub fn listening(&self) -> bool {
        let Ok(file) = File::open(self.path().join("delivery.lock")) else {
            return false;
        };
        // The lock goes with the file, at once.
        file.try_lock_shared().is_err()
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

/// What a state file looked like when last read: an atomic save replaces
/// the file, so a save changes it even within one tick of the clock.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    #[cfg(unix)]
    inode: u64,
}

impl Stamp {
    fn of(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        Some(Self {
            modified: metadata.modified().ok(),
            len: metadata.len(),
            #[cfg(unix)]
            inode: std::os::unix::fs::MetadataExt::ino(&metadata),
        })
    }
}

impl Store {
    /// A cheap look at the state, to read it again only when it changed.
    pub fn stamp(&self) -> Option<Stamp> {
        Stamp::of(&self.path().join("state.json"))
    }
}

pub fn discover(repo_root: &Path) -> Result<Vec<Curio>> {
    Discovery::default().refresh(repo_root)
}

/// [`discover`] for one who asks again and again: a session's curios are
/// read again only once its state changed.
#[derive(Debug, Default)]
pub struct Discovery {
    seen: BTreeMap<String, Seen>,
    /// The folders the last refresh could not read, by name: one stray
    /// folder leaves the others listed.
    pub skipped: Vec<(String, Error)>,
}

/// What a session's state held when [`Discovery`] last read it.
#[derive(Debug)]
struct Seen {
    stamp: Option<Stamp>,
    curios: Vec<Curio>,
    /// The ids of its curios with feedback queued.
    queued: BTreeSet<String>,
}

impl Discovery {
    pub fn refresh(&mut self, repo_root: &Path) -> Result<Vec<Curio>> {
        self.skipped.clear();
        let base = Directories::from_root(repo_root).curios();
        if !base.exists() {
            self.seen.clear();
            return Ok(vec![]);
        }
        no_symlinks(&base)?;
        let mut seen = BTreeMap::new();
        for item in fs::read_dir(base)?.take(1000) {
            let item = item?;
            let session = item.file_name().to_string_lossy().into_owned();
            match item.file_type() {
                Ok(kind) if !kind.is_dir() => continue,
                Ok(_) => {}
                Err(error) => {
                    self.skipped.push((session, error.into()));
                    continue;
                }
            }
            if let Err(error) = component(&session) {
                self.skipped.push((session, error));
                continue;
            }
            let store = Store {
                root: repo_root.to_owned(),
                session,
            };
            let stamp = store.stamp();
            let (curios, queued) = match self.seen.remove(&store.session) {
                Some(before) if before.stamp == stamp => (before.curios, before.queued),
                _ => match store.listing() {
                    Ok(listing) => listing,
                    Err(error) => {
                        self.skipped.push((store.session, error));
                        continue;
                    }
                },
            };
            seen.insert(
                store.session,
                Seen {
                    stamp,
                    curios,
                    queued,
                },
            );
        }
        self.seen = seen;
        Ok(self.seen.values().flat_map(|s| s.curios.clone()).collect())
    }

    /// The curios the last refresh found feedback queued for, as session
    /// and curio id: read with their lists, so asking costs no store.
    pub fn queued(&self) -> BTreeSet<(String, String)> {
        self.seen
            .iter()
            .flat_map(|(session, seen)| seen.queued.iter().map(|id| (session.clone(), id.clone())))
            .collect()
    }
}

/// Runs a store call on a blocking thread: one may wait on another
/// process's lock or spawn git, which would stall an async worker.
pub async fn offload<T: std::marker::Send + 'static>(
    f: impl FnOnce() -> Result<T> + std::marker::Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| Error::Invalid("curios worker failed".into()))?
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn owner_root_of_a_linked_worktree_is_the_main_checkout() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "--quiet"]);
        git(
            dir.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "--quiet",
                "--allow-empty",
                "-m",
                "initial",
            ],
        );
        let linked = dir.path().join("linked");
        git(
            dir.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "linked",
                linked.to_str().unwrap(),
            ],
        );
        let main = dir.path().canonicalize().unwrap();
        assert_eq!(owner_root(&linked).unwrap(), main);
        assert_eq!(owner_root(&linked.join(".")).unwrap(), main);
        let store = Store::open(&linked, "owner").unwrap();
        assert_eq!(store.repo_root(), main);
        assert!(store.template_path().is_file());
        let outside = tempfile::tempdir().unwrap();
        assert!(matches!(
            owner_root(outside.path()),
            Err(Error::RepoRequired)
        ));
    }
}
