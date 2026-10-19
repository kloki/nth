//! The file tabs, wired to the `show_user` tool: one per file the model
//! showed, following it on disk until you close it.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use tokio::sync::mpsc;

use super::{App, Tab};
use crate::file::{FileId, FileRead, FileView};

/// The file tabs' views and where their watchers report.
pub(super) struct FileTabs {
    /// Each file tab, open from when the model showed it until you close
    /// it, and the id the next one gets.
    pub(super) views: BTreeMap<FileId, FileView>,
    next: FileId,
    /// Where the file tabs' watchers send what they read.
    tx: mpsc::Sender<FileRead>,
    pub(super) rx: mpsc::Receiver<FileRead>,
}

impl FileTabs {
    pub(super) fn new() -> Self {
        let (tx, rx) = mpsc::channel(16);
        Self {
            views: BTreeMap::new(),
            next: 0,
            tx,
            rx,
        }
    }

    /// The next read of any tab's watcher. Never `None`, since the sender
    /// is kept here, so it waits while no tab is open.
    pub(super) async fn next(&mut self) -> Option<FileRead> {
        self.rx.recv().await
    }

    /// The tab showing `path`, opened when there is none yet.
    fn open(&mut self, path: PathBuf) -> FileId {
        let open = self
            .views
            .iter()
            .find(|(_, view)| view.path() == path)
            .map(|(&id, _)| id);
        open.unwrap_or_else(|| {
            let id = self.next;
            self.next += 1;
            let view = FileView::new(id, path, self.tx.clone());
            self.views.insert(id, view);
            id
        })
    }

    /// A tab's watcher read its file; one closed since is gone.
    fn read(&mut self, id: FileId, text: Option<String>) {
        if let Some(view) = self.views.get_mut(&id) {
            view.update(text);
        }
    }

    /// Drops the views of tabs no longer in `tabs`, which stops their
    /// watchers.
    fn keep_open(&mut self, tabs: &[Tab]) {
        self.views.retain(|&id, _| tabs.contains(&Tab::File(id)));
    }

    /// The file as its tab names it: relative to `cwd`.
    fn place(&self, id: FileId, cwd: &Path) -> String {
        let Some(view) = self.views.get(&id) else {
            return String::new();
        };
        let path = view.path();
        path.strip_prefix(cwd).unwrap_or(path).display().to_string()
    }
}

impl App {
    /// Shows the file at `path`, in the tab it has or a new one.
    pub(super) fn show_file(&mut self, path: PathBuf) {
        let id = self.file_tabs.open(path);
        self.content.open(Tab::File(id));
    }

    /// A file tab's watcher read its file.
    pub(super) fn file_read(&mut self, id: FileId, text: Option<String>) {
        self.file_tabs.read(id, text);
    }

    /// Drops the views of file tabs you closed.
    pub(super) fn forget_closed_files(&mut self) {
        self.file_tabs.keep_open(self.content.tabs());
    }

    /// The file as its tab names it: relative to the working directory.
    pub(super) fn file_place(&self, id: FileId) -> String {
        self.file_tabs.place(id, &self.cwd)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{
        keys::Action,
        tests::{app, rows},
    };

    async fn read(app: &mut App) {
        let (id, text) = app.file_tabs.rx.recv().await.expect("read");
        app.file_read(id, text);
    }

    #[tokio::test]
    async fn a_shown_file_opens_in_its_own_tab_and_follows_the_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notes.md");
        std::fs::write(&path, "# Notes\nfirst\n").expect("writes");
        let mut app = app();

        app.show_file(path.clone());
        read(&mut app).await;

        assert_eq!(app.content.active(), Tab::File(0));
        let shown = rows(&mut app);
        assert!(shown[0].contains("[◫ notes.md]"), "{shown:#?}");
        assert!(shown.iter().any(|r| r.trim() == "first"), "{shown:#?}");

        std::fs::write(&path, "# Notes\nsecond\n").expect("writes");
        read(&mut app).await;
        let shown = rows(&mut app);
        assert!(shown.iter().any(|r| r.trim() == "second"), "{shown:#?}");
    }

    #[tokio::test]
    async fn the_same_file_reuses_its_tab_and_another_gets_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (a, b) = (dir.path().join("a.rs"), dir.path().join("b.md"));
        std::fs::write(&a, "fn a() {}\n").expect("writes");
        std::fs::write(&b, "b\n").expect("writes");
        let mut app = app();

        app.show_file(a.clone());
        app.show_file(b);
        app.show_file(a);

        assert_eq!(app.content.tabs(), [Tab::Chat, Tab::File(0), Tab::File(1)]);
        assert_eq!(app.content.active(), Tab::File(0));
    }

    #[tokio::test]
    async fn closing_the_tab_drops_its_view() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.rs");
        std::fs::write(&path, "fn a() {}\n").expect("writes");
        let mut app = app();
        app.show_file(path);

        app.apply(Action::CloseContent);

        assert_eq!(app.content.tabs(), [Tab::Chat]);
        assert!(app.file_tabs.views.is_empty());
    }
}
