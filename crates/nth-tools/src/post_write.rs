//! What happens after a tool writes a file: its formatters run, its
//! language servers check it, and the model is told what they found. The
//! tools write and format holding [`PostWrite::lock`] and ask the servers
//! after letting it go, each in its own way, so they call the two halves.

use std::{
    collections::HashMap,
    fs::{File, OpenOptions, Permissions},
    io::{self, ErrorKind, Write as _},
    path::{Component, Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex as StdMutex, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use nth_format::{Formatters, Outcome};
use nth_lsp::{Caller, Diagnostic, Lsp, report};
use nth_protocol::ToolContext;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::bom;

static EDITS: LazyLock<StdMutex<HashMap<PathBuf, Weak<Mutex<()>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// The symlinks a dangling chain may follow before it is left unresolved.
const MAX_LINKS: usize = 40;

/// Resolve existing symlinks, retaining missing components under the nearest
/// existing parent. A dangling symlink resolves to its target, so a link and
/// the target it is about to gain share one identity. Resolve `..` only after
/// following any preceding symlink. Blocking: it asks the filesystem about
/// each component.
pub(crate) fn identity(path: &Path) -> PathBuf {
    resolve(path, 0)
}

fn resolve(path: &Path, followed: usize) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            other => {
                result.push(other.as_os_str());
                match std::fs::canonicalize(&result) {
                    Ok(canonical) => result = canonical,
                    Err(_) if followed < MAX_LINKS => {
                        // `canonicalize` stops at a symlink whose target is
                        // missing; read the link ourselves, relative to its
                        // parent, bounded so a cycle ends unresolved.
                        if let Ok(target) = std::fs::read_link(&result) {
                            let parent = result.parent().unwrap_or(Path::new("/"));
                            result = resolve(&parent.join(target), followed + 1);
                        }
                    }
                    Err(_) => {}
                }
            }
        }
    }
    result
}

#[derive(Clone)]
pub struct PostWrite {
    format: Arc<Formatters>,
    lsp: Lsp,
}

impl PostWrite {
    pub fn new(format: Arc<Formatters>, lsp: Lsp) -> Self {
        Self { format, lsp }
    }

    pub fn lsp(&self) -> &Lsp {
        &self.lsp
    }

    pub fn formatters(&self) -> &Arc<Formatters> {
        &self.format
    }

    /// Acquire all identities in order before reading or writing any of them.
    pub(crate) async fn lock(&self, paths: &[PathBuf]) -> Writing<'_> {
        let mut identities: Vec<_> = paths.iter().map(|path| identity(path)).collect();
        identities.sort();
        identities.dedup();
        let locks: Vec<_> = {
            let mut registry = EDITS.lock().unwrap_or_else(|e| e.into_inner());
            registry.retain(|_, lock| lock.strong_count() > 0);
            identities
                .into_iter()
                .map(|path| {
                    let entry = registry.entry(path).or_default();
                    entry.upgrade().unwrap_or_else(|| {
                        let lock = Arc::new(Mutex::new(()));
                        *entry = Arc::downgrade(&lock);
                        lock
                    })
                })
                .collect()
        };
        let mut held = Vec::with_capacity(locks.len());
        for lock in locks {
            held.push(lock.lock_owned().await);
        }
        Writing {
            post_write: self,
            _held: held,
        }
    }

    /// The errors language servers report on `path` as it now is, and on
    /// up to five other files. Called after formatting, so positions match
    /// the file as it now is.
    pub(crate) async fn check(&self, path: &Path, caller: Option<Caller>) -> String {
        let diagnostics = self.touch(path, caller).await;
        report::after_write(path, &diagnostics)
    }

    /// What the language servers report on `path` once they have seen it
    /// as it now is; other files' diagnostics are left out.
    pub(crate) async fn diagnostics(&self, path: &Path, caller: Option<Caller>) -> Vec<Diagnostic> {
        let mut all = self.touch(path, caller).await;
        all.remove(path).unwrap_or_default()
    }
    async fn touch(
        &self,
        path: &Path,
        caller: Option<Caller>,
    ) -> std::collections::BTreeMap<std::path::PathBuf, Vec<Diagnostic>> {
        match caller {
            Some(caller) => self.lsp.touch_attributed(path, true, caller).await,
            None => self.lsp.touch(path, true).await,
        }
    }
}

pub(crate) fn caller(ctx: &ToolContext) -> Option<Caller> {
    if ctx.session.is_empty() {
        None
    } else {
        Some(Caller {
            session: ctx.session.clone(),
            is_subagent: ctx.is_subagent,
            agent: ctx.agent.as_ref().map(|a| a.name.clone()),
        })
    }
}

/// The write lock, held from before a tool reads what it changes until
/// the formatters are done with it. The formatter rewrites the file, so it
/// runs under the lock; waiting on the language servers need not, so the
/// tools drop the guard before asking them.
pub(crate) struct Writing<'a> {
    post_write: &'a PostWrite,
    _held: Vec<OwnedMutexGuard<()>>,
}

impl Drop for Writing<'_> {
    fn drop(&mut self) {
        self._held.clear();
        EDITS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, lock| lock.strong_count() > 0);
    }
}

impl Writing<'_> {
    /// Formats `path` with the formatters for `cwd`'s project and returns
    /// one note per formatter that ran. A BOM the file had before survives
    /// the formatter.
    pub(crate) async fn format(&self, path: &Path, cwd: &Path) -> String {
        let bom = bom::has_bom(path).await.unwrap_or(false);
        let outcomes = self.post_write.format.format(path, cwd).await;
        let mut notes: Vec<_> = outcomes.iter().map(note).collect();
        if !outcomes.is_empty()
            && let Err(e) = bom::sync(path, bom).await
        {
            notes.push(format!("Could not restore the byte order mark: {e}"));
        }
        notes.join("\n")
    }
}

/// Writes `bytes` to `path` aside and renames them over it, so a reader
/// running beside, a crash or a full disk never leaves half a file; missing
/// folders are created. A symlink is written through: the file it names is
/// replaced and the link stays a link. A file that exists keeps its
/// permissions, and one that refuses to be written in place still refuses,
/// though the rename itself asks only the folder. Where no rename can work,
/// a file in a folder nth may not write in, or a file mounted on its own,
/// the file is written in place.
pub(crate) async fn write_atomic(path: &Path, bytes: Vec<u8>) -> io::Result<()> {
    let path = path.to_path_buf();
    // One blocking job rather than a `tokio::fs` call per step: a call
    // dropped between steps would otherwise leave its temporary file.
    tokio::task::spawn_blocking(move || write_aside(&path, &bytes))
        .await
        .map_err(io::Error::other)?
}

fn write_aside(path: &Path, bytes: &[u8]) -> io::Result<()> {
    // The rename must land on the file a symlink names, not on the link.
    let target = identity(path);
    let existing = match std::fs::metadata(&target) {
        Ok(meta) => Some(meta),
        Err(e) if e.kind() == ErrorKind::NotFound => {
            if std::fs::symlink_metadata(&target).is_ok() {
                // `identity` stops following a chain that loops or runs too
                // long, leaving the link itself, which a rename would replace.
                return Err(io::Error::other("too many levels of symbolic links"));
            }
            None
        }
        Err(e) => return Err(e),
    };
    if let Some(meta) = &existing {
        may_write(&target, meta)?;
    }
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "no parent folder"))?;
    let permissions = existing.as_ref().map(|meta| meta.permissions());
    let (tmp, file) = match create_temp(parent, permissions.as_ref()) {
        Ok(created) => created,
        Err(e) if e.kind() == ErrorKind::PermissionDenied && existing.is_some() => {
            return write_in_place(&target, bytes);
        }
        Err(e) => return Err(e),
    };
    match replace(&tmp, file, bytes, &target) {
        // A file bind-mounted on its own (a devcontainer's) cannot be
        // renamed over, nor can one on another filesystem than its folder.
        Err(e)
            if matches!(
                e.kind(),
                ErrorKind::ResourceBusy | ErrorKind::CrossesDevices
            ) =>
        {
            write_in_place(&target, bytes)
        }
        result => result,
    }
}

/// Refuses a file that writing in place would have refused: a directory,
/// or one this process may not write. The rename would only ask the folder.
fn may_write(target: &Path, meta: &std::fs::Metadata) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    if meta.is_dir() {
        return Err(io::Error::from(ErrorKind::IsADirectory));
    }
    // Asked rather than opened for writing, which a watcher would take for
    // a write.
    let c_path = std::ffi::CString::new(target.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(ErrorKind::InvalidInput, e))?;
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the
    // call, and access only reads it.
    if unsafe { libc::access(c_path.as_ptr(), libc::W_OK) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Numbers the temporary files: unique per write, as two calls may write
/// into one folder at once, and short, so a long file name cannot push the
/// temporary one past the name limit.
static WRITES: AtomicU64 = AtomicU64::new(0);

/// A new, empty file in `parent` to write aside in, with the folder made if
/// it is missing. Given the permissions of the file it replaces, it has them
/// before it holds a byte, so a private file's content is never readable by
/// anyone it was not readable by.
fn create_temp(parent: &Path, permissions: Option<&Permissions>) -> io::Result<(PathBuf, File)> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    // A name may be taken by a file a killed run of nth left behind.
    const TRIES: usize = 16;
    let mut made_folder = false;
    let mut tries = 0;
    loop {
        let n = WRITES.fetch_add(1, Ordering::Relaxed);
        let tmp = parent.join(format!(".nth-{}-{n}.tmp", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        if let Some(permissions) = permissions {
            // The umask may take bits away, never add them, so the file is
            // never more open than the one it replaces.
            options.mode(permissions.mode() & 0o777);
        }
        match options.open(&tmp) {
            Ok(file) => {
                // Exactly as the file it replaces, bits the umask took
                // included, still before any byte is written.
                if let Some(permissions) = permissions
                    && let Err(e) = file.set_permissions(permissions.clone())
                {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e);
                }
                return Ok((tmp, file));
            }
            Err(e) if e.kind() == ErrorKind::NotFound && !made_folder => {
                std::fs::create_dir_all(parent)?;
                made_folder = true;
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists && tries < TRIES => tries += 1,
            Err(e) => return Err(e),
        }
    }
}

/// Fills `tmp` with `bytes` and renames it over `target`; on any failure the
/// temporary file goes and `target` is as it was.
fn replace(tmp: &Path, mut file: File, bytes: &[u8], target: &Path) -> io::Result<()> {
    let written = (|| {
        file.write_all(bytes)?;
        // On disk before the rename, so a power cut cannot leave the new
        // name pointing at an empty file.
        file.sync_all()?;
        std::fs::rename(tmp, target)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    written
}

/// The last resort when no rename can replace `target`: truncated and
/// written where it is, which keeps its permissions by itself.
fn write_in_place(target: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).truncate(true).open(target)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// `result`, then each section that is not empty, a blank line apart. LSP
/// reports come with leading blank lines of their own, which go.
pub(crate) fn append(result: String, sections: &[&str]) -> String {
    let mut out = result;
    for section in sections {
        let section = section.trim_start_matches('\n');
        if !section.is_empty() {
            out.push_str("\n\n");
            out.push_str(section);
        }
    }
    out
}

/// The format notes, then the LSP report, a blank line apart.
#[cfg(test)]
fn join_sections(notes: &str, errors: &str) -> String {
    let joined = append(notes.to_string(), &[errors]);
    joined.trim_start_matches('\n').to_string()
}

fn note(outcome: &Outcome) -> String {
    match &outcome.result {
        Ok(()) => format!("Formatted with {}.", outcome.name),
        Err(e) => format!("{} failed: {e}", outcome.name),
    }
}

#[cfg(test)]
impl PostWrite {
    /// What the write tool appends after writing `path`: a note per
    /// formatter that ran, then the errors language servers report on the
    /// formatted file (and on up to five others), a blank line apart. Empty
    /// when nothing ran and nothing is wrong.
    async fn after_write(&self, path: &Path, cwd: &Path) -> String {
        let writing = self.lock(&[path.to_path_buf()]).await;
        let notes = writing.format(path, cwd).await;
        drop(writing);
        let errors = self.check(path, None).await;
        join_sections(&notes, &errors)
    }

    /// Formats nothing and checks nothing.
    pub(crate) fn off() -> Self {
        let config = nth_format::FormatConfig {
            enabled: false,
            ..Default::default()
        };
        Self::new(Arc::new(Formatters::new(&config)), lsp_off())
    }

    /// Runs `command` on `.txt` files and on nothing else, with no language
    /// servers.
    pub(crate) fn with_formatter(name: &str, command: &[&str]) -> Self {
        Self::new(formatter(name, command, ".txt"), lsp_off())
    }

    /// Formats nothing and asks every language server on PATH.
    pub(crate) fn lsp_only() -> Self {
        let format = nth_format::FormatConfig {
            enabled: false,
            ..Default::default()
        };
        let lsp = Lsp::new(&nth_lsp::LspConfig::default());
        Self::new(Arc::new(Formatters::new(&format)), lsp)
    }
}

/// A cargo project in `dir` whose `src/main.rs` is `main` and whose
/// `src/other.rs` has a type error, for tests with a real rust-analyzer.
#[cfg(test)]
pub(crate) fn broken_crate(dir: &Path) {
    std::fs::create_dir_all(dir.join("src")).expect("mkdir");
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"broken\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .expect("write");
    let main = "mod other;\n\nfn main() {\n    let x: u32 = 1;\n    println!(\"{x}\");\n}\n";
    std::fs::write(dir.join("src/main.rs"), main).expect("write");
    let other = "pub fn f() -> u32 {\n    \"text\"\n}\n";
    std::fs::write(dir.join("src/other.rs"), other).expect("write");
}

/// Only the formatter `name`, on files with `extension`.
#[cfg(test)]
fn formatter(name: &str, command: &[&str], extension: &str) -> Arc<Formatters> {
    let mut config = nth_format::FormatConfig::default();
    let entry = nth_format::FormatterConfig {
        command: Some(command.iter().map(|s| s.to_string()).collect()),
        extensions: Some(vec![extension.into()]),
        ..Default::default()
    };
    config.formatters.insert(name.into(), entry);
    Arc::new(Formatters::new(&config))
}

/// Starts no language server.
#[cfg(test)]
pub(crate) fn lsp_off() -> Lsp {
    Lsp::new(&nth_lsp::LspConfig {
        enabled: false,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bom::BOM;

    #[tokio::test]
    async fn path_locks_exclude_aliases_but_not_other_paths_and_release_on_cancel() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::write(&a, "a").unwrap();
        let post = PostWrite::off();
        let held = post.lock(std::slice::from_ref(&a)).await;
        // `a/../a` does not resolve at the OS level, since a file cannot be
        // traversed, but the identity folds it back to `a`: the spelling is
        // conservatively over-blocked.
        for alias in [dir.path().join(".").join("a"), a.join("..").join("a")] {
            let waiting = post.lock(std::slice::from_ref(&alias));
            tokio::pin!(waiting);
            assert!(
                futures::poll!(&mut waiting).is_pending(),
                "{}",
                alias.display()
            );
            // Dropping the pinned future here cancels the acquisition.
        }
        // Another path is not held up by the first.
        let other = post.lock(std::slice::from_ref(&b)).await;
        drop(other);
        drop(held);
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            post.lock(std::slice::from_ref(&a)),
        )
        .await
        .expect("a cancelled acquisition releases its path");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_aliases_and_missing_descendants_share_locks() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        std::os::unix::fs::symlink("real", dir.path().join("link")).unwrap();
        let post = PostWrite::off();
        let real = dir.path().join("real/missing/file");
        let link = dir.path().join("link/missing/file");
        let held = post.lock(std::slice::from_ref(&real)).await;
        let waiting = post.lock(std::slice::from_ref(&link));
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        drop(held);
        waiting.await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dangling_symlink_aliases_its_future_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink("target", &link).unwrap();
        let post = PostWrite::off();
        let held = post.lock(std::slice::from_ref(&link)).await;
        let waiting = post.lock(std::slice::from_ref(&target));
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        drop(held);
        waiting.await;
    }

    #[tokio::test]
    async fn overlapping_sets_are_ordered_and_cancelled_acquisitions_release() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let post = PostWrite::off();
        let held = post.lock(std::slice::from_ref(&b)).await;
        {
            let both = [b.clone(), a.clone()];
            let waiting = post.lock(&both);
            tokio::pin!(waiting);
            assert!(futures::poll!(&mut waiting).is_pending());
        }
        let a_guard = post.lock(std::slice::from_ref(&a)).await;
        drop(a_guard);
        drop(held);
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(
                async {
                    let _guard = post.lock(&[a.clone(), b.clone()]).await;
                },
                async {
                    let _guard = post.lock(&[b.clone(), a.clone()]).await;
                }
            )
        })
        .await
        .expect("overlapping acquisitions in opposite orders both finish");
        assert_eq!((first, second), ((), ()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_write_never_leaves_a_half_file() {
        use std::sync::atomic::AtomicBool;

        const SIZE: usize = 1 << 20;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big");
        std::fs::write(&path, vec![b'a'; SIZE]).unwrap();
        let done = Arc::new(AtomicBool::new(false));
        let reader = {
            let (path, done) = (path.clone(), done.clone());
            std::thread::spawn(move || {
                let mut reads = 0;
                while !done.load(Ordering::Relaxed) {
                    let seen = std::fs::read(&path).expect("the file is always there");
                    let whole = seen.len() == SIZE
                        && (seen.iter().all(|&b| b == b'a') || seen.iter().all(|&b| b == b'b'));
                    assert!(whole, "a read saw {} bytes of a half write", seen.len());
                    reads += 1;
                }
                reads
            })
        };
        for round in 0..40 {
            let byte = if round % 2 == 0 { b'b' } else { b'a' };
            write_atomic(&path, vec![byte; SIZE]).await.unwrap();
        }
        done.store(true, Ordering::Relaxed);
        let reads = reader.join().expect("no read saw half a file");
        assert!(reads > 0);
        let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "no temporary file is left behind");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_atomic_write_keeps_the_mode_and_writes_through_symlinks() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("run.sh");
        std::fs::write(&script, "old").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o750)).unwrap();
        write_atomic(&script, b"new".to_vec()).await.unwrap();
        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o750);

        // Through a link to a file, and a dangling one: the link stays and
        // the file it names gets the bytes.
        let link = dir.path().join("link");
        symlink("run.sh", &link).unwrap();
        write_atomic(&link, b"via link".to_vec()).await.unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(std::fs::read_to_string(&script).unwrap(), "via link");
        let dangling = dir.path().join("dangling");
        symlink("sub/target", &dangling).unwrap();
        write_atomic(&dangling, b"made".to_vec()).await.unwrap();
        assert!(
            std::fs::symlink_metadata(&dangling)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("sub/target")).unwrap(),
            "made"
        );

        // A loop has no file to write, and the link is left alone.
        let looped = dir.path().join("loop");
        symlink("loop", &looped).unwrap();
        write_atomic(&looped, b"x".to_vec()).await.unwrap_err();
        assert!(
            std::fs::symlink_metadata(&looped)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        // A file that may not be written in place is refused; root may
        // write anything, so it is not asked there.
        let locked = dir.path().join("locked");
        std::fs::write(&locked, "kept").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o444)).unwrap();
        if !is_root() {
            let err = write_atomic(&locked, b"x".to_vec()).await.unwrap_err();
            assert_eq!(err.kind(), ErrorKind::PermissionDenied);
            assert_eq!(std::fs::read_to_string(&locked).unwrap(), "kept");
        }
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(
            names
                .iter()
                .all(|name| !name.to_string_lossy().ends_with(".tmp")),
            "{names:?}"
        );
    }

    fn is_root() -> bool {
        // SAFETY: geteuid has no preconditions and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }

    #[test]
    fn the_temporary_file_has_the_old_mode_before_it_holds_a_byte() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        // 0600 must never be wider; 0666 must not lose what the umask takes.
        for mode in [0o600, 0o666] {
            let permissions = Permissions::from_mode(mode);
            let (tmp, _file) = create_temp(dir.path(), Some(&permissions)).unwrap();
            let meta = std::fs::metadata(&tmp).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, mode);
            assert_eq!(meta.len(), 0);
            std::fs::remove_file(tmp).unwrap();
        }
    }

    #[test]
    fn a_failed_rename_takes_its_temporary_file_with_it() {
        let dir = tempfile::tempdir().unwrap();
        // A folder with something in it cannot be renamed over.
        let target = dir.path().join("folder");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("inside"), "kept").unwrap();
        let (tmp, file) = create_temp(dir.path(), None).unwrap();
        replace(&tmp, file, b"new", &target).unwrap_err();
        assert!(!tmp.exists());
        assert_eq!(
            std::fs::read_to_string(target.join("inside")).unwrap(),
            "kept"
        );
    }

    #[tokio::test]
    async fn a_leftover_temporary_file_is_stepped_over() {
        let dir = tempfile::tempdir().unwrap();
        // The next names this process will try, as a killed run that had
        // the same pid would have left them.
        let next = WRITES.load(Ordering::Relaxed);
        for n in next..next + 3 {
            let name = format!(".nth-{}-{n}.tmp", std::process::id());
            std::fs::write(dir.path().join(name), "left").unwrap();
        }
        let path = dir.path().join("a");
        write_atomic(&path, b"new".to_vec()).await.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_in_a_folder_nth_may_not_write_is_written_in_place() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if is_root() {
            // Root writes into any folder, so the rename always works.
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("locked");
        std::fs::create_dir(&folder).unwrap();
        let path = folder.join("a");
        std::fs::write(&path, "old").unwrap();
        let inode = std::fs::metadata(&path).unwrap().ino();
        std::fs::set_permissions(&folder, Permissions::from_mode(0o555)).unwrap();
        let written = write_atomic(&path, b"new".to_vec()).await;
        std::fs::set_permissions(&folder, Permissions::from_mode(0o755)).unwrap();
        written.unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(
            std::fs::metadata(&path).unwrap().ino(),
            inode,
            "written in place"
        );
        // A new file there has nowhere to go.
        std::fs::set_permissions(&folder, Permissions::from_mode(0o555)).unwrap();
        let refused = write_atomic(&folder.join("b"), b"x".to_vec()).await;
        std::fs::set_permissions(&folder, Permissions::from_mode(0o755)).unwrap();
        assert_eq!(refused.unwrap_err().kind(), ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn notes_each_formatter_that_ran() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a").expect("write");
        let post = PostWrite::with_formatter("sed", &["perl", "-pi", "-e", "s/a/b/", "$FILE"]);

        let note = post.after_write(&path, dir.path()).await;

        assert_eq!(note, "Formatted with sed.");
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "b");
    }

    #[tokio::test]
    async fn notes_a_failed_formatter_in_one_line() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a").expect("write");
        let post =
            PostWrite::with_formatter("bad", &["sh", "-c", "echo 'unexpected token' >&2; exit 1"]);

        let note = post.after_write(&path, dir.path()).await;

        assert_eq!(note, "bad failed: unexpected token");
    }

    #[tokio::test]
    async fn says_nothing_when_no_formatter_matches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.cs");
        std::fs::write(&path, "a").expect("write");
        let post = PostWrite::with_formatter("sed", &["perl", "-pi", "-e", "s/a/b/", "$FILE"]);

        assert_eq!(post.after_write(&path, dir.path()).await, "");
    }

    #[tokio::test]
    async fn bom_survives_a_formatter_that_drops_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, format!("{BOM}a")).expect("write");
        let post = PostWrite::with_formatter("rewrite", &["sh", "-c", "printf b > $FILE"]);

        post.after_write(&path, dir.path()).await;

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            format!("{BOM}b")
        );
    }

    #[tokio::test]
    async fn formatter_does_not_add_a_bom_the_file_lacked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "a").expect("write");
        let script = format!("printf '{BOM}b' > $FILE");
        let post = PostWrite::with_formatter("rewrite", &["sh", "-c", &script]);

        post.after_write(&path, dir.path()).await;

        assert_eq!(std::fs::read_to_string(&path).expect("read"), "b");
    }

    #[test]
    fn format_notes_come_before_lsp_errors() {
        let errors = "\n\nLSP errors detected in this file, please fix:\n<diagnostics>";
        assert_eq!(
            join_sections("Formatted with rustfmt.", errors),
            "Formatted with rustfmt.\n\nLSP errors detected in this file, please fix:\n<diagnostics>"
        );
        assert_eq!(
            join_sections("", errors),
            "LSP errors detected in this file, please fix:\n<diagnostics>"
        );
        assert_eq!(join_sections("Formatted with x.", ""), "Formatted with x.");
        assert_eq!(join_sections("", ""), "");
    }

    /// The whole path with a real rust-analyzer: run it by hand with
    /// `cargo test -p nth-tools -- --ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs rust-analyzer on PATH"]
    async fn reports_rust_analyzer_errors_after_the_format_note() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path();
        std::fs::create_dir_all(project.join("src")).expect("mkdir");
        std::fs::write(
            project.join("Cargo.toml"),
            "[package]\nname = \"broken\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("write");
        let path = project.join("src/main.rs");
        std::fs::write(&path, "fn main() {\n    let x: u32 = \"text\";\n}\n").expect("write");
        let post = PostWrite::new(
            formatter("noop", &["true"], ".rs"),
            Lsp::new(&nth_lsp::LspConfig::default()),
        );

        // rust-analyzer only checks once it has loaded the crate, which can
        // take longer than one write waits.
        let mut out = String::new();
        for _ in 0..30 {
            out = post.after_write(&path, project).await;
            if out.contains("LSP errors") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        println!("{out}");
        let note = out.find("Formatted with noop.").expect("format note");
        let errors = out
            .find("\n\nLSP errors detected in this file, please fix:\n")
            .expect("lsp errors");
        assert!(note < errors, "{out}");
        assert!(out.contains("ERROR [2:"), "{out}");
    }
}
