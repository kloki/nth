//! What changed in the instruction files since the model read them. A file
//! edited mid-session reaches the model as a change at its next step,
//! as opencode does, rather than when the session restarts; the system
//! prompt keeps the text it started with, so its cache holds.
//!
//! Looked at with a stat per file when asked, not watched: a session has a
//! handful of them and asks once per step.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use crate::Context;

/// The instruction files of one session and what the model last saw of
/// each. Cheap to clone, and clones share what they saw. The default looks
/// at nothing, for a turn that never hears of changes.
#[derive(Debug, Clone, Default)]
pub struct Watch(Option<Arc<Mutex<Files>>>);

#[derive(Debug, Clone, Default)]
struct Files(BTreeMap<PathBuf, Seen>);

/// What the model saw of one file: empty for none.
#[derive(Debug, Clone)]
struct Seen {
    /// When the file was last looked at, and its length; `None` until it
    /// has been, or while it is absent.
    stamp: Option<(SystemTime, u64)>,
    content: String,
}

/// One file the model's copy of is out of date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub path: PathBuf,
    pub kind: ChangeKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeKind {
    /// It appeared where it would have been read.
    Created(String),
    /// What it said, and what it says now.
    Edited {
        before: String,
        after: String,
    },
    Removed,
}

impl ChangeKind {
    /// The `change` attribute of its notice.
    pub fn name(&self) -> &'static str {
        match self {
            ChangeKind::Created(_) => "created",
            ChangeKind::Edited { .. } => "edited",
            ChangeKind::Removed => "removed",
        }
    }
}

impl Watch {
    /// Looks at the files `context` put in the system prompt, as the model
    /// saw them, and at where one more could appear.
    pub fn new(context: &Context) -> Self {
        let mut files = BTreeMap::new();
        for instruction in &context.instructions {
            files.insert(instruction.path.clone(), Seen::new(&instruction.content));
        }
        for path in &context.absent_instructions {
            files.entry(path.clone()).or_insert_with(|| Seen::new(""));
        }
        Self(Some(Arc::new(Mutex::new(Files(files)))))
    }

    /// What changed since the last call. `loaded` is every file in the
    /// conversation; one not looked at yet was just attached as it is now,
    /// so it is taken as seen.
    pub async fn changes(&self, loaded: BTreeSet<PathBuf>) -> Vec<Change> {
        let Some(files) = self.0.clone() else {
            return Vec::new();
        };
        crate::blocking(move || {
            files
                .lock()
                .expect("only poisoned if a holder panicked")
                .changes(loaded)
        })
        .await
    }

    /// Inspect the same changes delivery would report, without advancing the
    /// shared watch. Newly attached files are treated as seen, as in delivery.
    /// This reads the watched files synchronously; callers should refresh at
    /// bounded intervals rather than on every screen draw.
    pub fn preview(&self, loaded: BTreeSet<PathBuf>) -> Vec<Change> {
        let Some(files) = &self.0 else {
            return Vec::new();
        };
        let mut snapshot = files
            .lock()
            .expect("only poisoned if a holder panicked")
            .clone();
        snapshot.changes(loaded)
    }
}

/// The same watch, sharing what it saw; two that look at nothing are equal.
impl PartialEq for Watch {
    fn eq(&self, other: &Self) -> bool {
        match (&self.0, &other.0) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (a, b) => a.is_none() && b.is_none(),
        }
    }
}

impl Seen {
    fn new(content: &str) -> Self {
        Self {
            stamp: None,
            content: content.to_string(),
        }
    }
}

impl Files {
    fn changes(&mut self, loaded: BTreeSet<PathBuf>) -> Vec<Change> {
        for path in loaded {
            self.0.entry(path).or_insert_with_key(|path| Seen {
                stamp: stamp(path),
                content: read(path),
            });
        }
        let mut changes = Vec::new();
        for (path, seen) in &mut self.0 {
            let now = stamp(path);
            if now.is_some() && now == seen.stamp {
                continue;
            }
            seen.stamp = now;
            let content = read(path);
            if content == seen.content {
                continue;
            }
            let before = std::mem::replace(&mut seen.content, content);
            let kind = match (before.trim().is_empty(), seen.content.trim().is_empty()) {
                (true, true) => continue,
                (true, false) => ChangeKind::Created(seen.content.clone()),
                (false, true) => ChangeKind::Removed,
                (false, false) => ChangeKind::Edited {
                    before,
                    after: seen.content.clone(),
                },
            };
            changes.push(Change {
                path: path.clone(),
                kind,
            });
        }
        changes
    }
}

fn stamp(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// The file's text, empty when it is gone or unreadable: either way the
/// model has nothing of it to go by.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::instructions::Instruction;

    fn context(dir: &Path, files: &[(&str, &str)], absent: &[&str]) -> Context {
        Context {
            instructions: files
                .iter()
                .map(|(name, content)| {
                    let path = dir.join(name);
                    fs::write(&path, content).expect("writes");
                    Instruction {
                        path,
                        content: content.to_string(),
                    }
                })
                .collect(),
            absent_instructions: absent.iter().map(|name| dir.join(name)).collect(),
            ..Context::default()
        }
    }

    fn kinds(changes: &[Change]) -> Vec<(String, &'static str)> {
        changes
            .iter()
            .map(|c| {
                let name = c.path.file_name().expect("named");
                (name.to_string_lossy().into_owned(), c.kind.name())
            })
            .collect()
    }

    #[tokio::test]
    async fn nothing_changed_says_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let watch = Watch::new(&context(dir.path(), &[("AGENTS.md", "a")], &[]));

        assert_eq!(watch.changes(BTreeSet::new()).await, []);
        assert_eq!(watch.changes(BTreeSet::new()).await, []);
    }

    #[tokio::test]
    async fn an_edit_is_told_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let watch = Watch::new(&context(dir.path(), &[("AGENTS.md", "old")], &[]));
        fs::write(dir.path().join("AGENTS.md"), "new").expect("writes");

        assert_eq!(
            watch.changes(BTreeSet::new()).await,
            [Change {
                path: dir.path().join("AGENTS.md"),
                kind: ChangeKind::Edited {
                    before: "old".into(),
                    after: "new".into()
                },
            }]
        );
        assert_eq!(watch.changes(BTreeSet::new()).await, []);
    }

    #[tokio::test]
    async fn preview_does_not_advance_the_watch() {
        let dir = tempfile::tempdir().unwrap();
        let watch = Watch::new(&context(
            dir.path(),
            &[("AGENTS.md", "old")],
            &["CLAUDE.md"],
        ));
        fs::write(dir.path().join("AGENTS.md"), "new").unwrap();
        fs::write(dir.path().join("CLAUDE.md"), "created").unwrap();
        let preview = watch.preview(BTreeSet::new());
        assert_eq!(preview.len(), 2);
        assert_eq!(watch.preview(BTreeSet::new()), preview);
        assert_eq!(watch.changes(BTreeSet::new()).await, preview);
        assert!(watch.preview(BTreeSet::new()).is_empty());
    }

    #[tokio::test]
    async fn a_file_where_one_would_be_read_is_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let watch = Watch::new(&context(dir.path(), &[], &["AGENTS.md"]));
        assert_eq!(watch.changes(BTreeSet::new()).await, []);

        fs::write(dir.path().join("AGENTS.md"), "fresh").expect("writes");
        let changes = watch.changes(BTreeSet::new()).await;
        assert_eq!(
            changes[0].kind,
            ChangeKind::Created("fresh".into()),
            "{changes:?}"
        );
    }

    #[tokio::test]
    async fn a_removed_file_is_told_and_may_come_back() {
        let dir = tempfile::tempdir().expect("tempdir");
        let watch = Watch::new(&context(dir.path(), &[("AGENTS.md", "a")], &[]));
        fs::remove_file(dir.path().join("AGENTS.md")).expect("removes");

        let changes = watch.changes(BTreeSet::new()).await;
        assert_eq!(kinds(&changes), [("AGENTS.md".into(), "removed")]);

        fs::write(dir.path().join("AGENTS.md"), "back").expect("writes");
        let changes = watch.changes(BTreeSet::new()).await;
        assert_eq!(kinds(&changes), [("AGENTS.md".into(), "created")]);
    }

    #[tokio::test]
    async fn a_file_attached_later_counts_from_then() {
        let dir = tempfile::tempdir().expect("tempdir");
        let watch = Watch::new(&context(dir.path(), &[], &[]));
        let nested = dir.path().join("CLAUDE.md");
        fs::write(&nested, "as attached").expect("writes");

        let loaded = BTreeSet::from([nested.clone()]);
        assert_eq!(watch.changes(loaded.clone()).await, []);
        fs::write(&nested, "edited since").expect("writes");
        let changes = watch.changes(loaded).await;
        assert_eq!(kinds(&changes), [("CLAUDE.md".into(), "edited")]);
    }

    #[tokio::test]
    async fn the_default_looks_at_nothing() {
        assert_eq!(Watch::default().changes(BTreeSet::new()).await, []);
    }
}
