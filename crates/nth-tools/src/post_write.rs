//! What happens after a tool writes a file: its formatters run, its
//! language servers check it, and the model is told what they found. The
//! tools write and format holding [`PostWrite::lock`] and ask the servers
//! after letting it go, each in its own way, so they call the two halves.

use std::{
    collections::HashMap,
    io::ErrorKind,
    path::{Component, Path, PathBuf},
    sync::{Arc, LazyLock, Mutex as StdMutex, Weak},
};

use nth_format::{Formatters, Outcome};
use nth_lsp::{Caller, Diagnostic, Lsp, report};
use nth_protocol::ToolContext;
use tokio::sync::{Mutex, OwnedMutexGuard};

use crate::bom;

static EDITS: LazyLock<StdMutex<HashMap<PathBuf, Weak<Mutex<()>>>>> =
    LazyLock::new(|| StdMutex::new(HashMap::new()));

/// Resolve existing symlinks, retaining missing components under the nearest
/// existing parent. Resolve `..` only after following any preceding symlink.
fn identity(path: &Path) -> PathBuf {
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
                if let Ok(canonical) = std::fs::canonicalize(&result) {
                    result = canonical;
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

/// Writes `content` to `path`, creating its folders when they are missing.
pub(crate) async fn write_with_dirs(path: &Path, content: &[u8]) -> std::io::Result<()> {
    match tokio::fs::write(path, content).await {
        Err(e) if e.kind() == ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            tokio::fs::write(path, content).await
        }
        result => result,
    }
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
        // `a/../a` is the same file even though `a` is not a directory: the
        // identity resolves `..` after the components before it.
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
        let (first, second) = tokio::join!(
            async {
                let _guard = post.lock(&[a.clone(), b.clone()]).await;
            },
            async {
                let _guard = post.lock(&[b.clone(), a.clone()]).await;
            }
        );
        assert_eq!((first, second), ((), ()));
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
