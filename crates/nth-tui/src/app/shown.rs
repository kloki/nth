//! The file tabs, wired to the `show_user` tool: one per file the model
//! showed, following it on disk until you close it.

use std::path::PathBuf;

use super::{App, Tab};
use crate::file::{FileId, FileView};

impl App {
    /// Shows the file at `path`, in the tab it has or a new one.
    pub(super) fn show_file(&mut self, path: PathBuf) {
        let open = self
            .file_views
            .iter()
            .find(|(_, view)| view.path() == path)
            .map(|(&id, _)| id);
        let id = open.unwrap_or_else(|| {
            let id = self.next_file;
            self.next_file += 1;
            let view = FileView::new(id, path, self.file_tx.clone());
            self.file_views.insert(id, view);
            id
        });
        self.content.open(Tab::File(id));
    }

    /// A file tab's watcher read its file; one closed since is gone.
    pub(super) fn file_read(&mut self, id: FileId, text: Option<String>) {
        if let Some(view) = self.file_views.get_mut(&id) {
            view.update(text);
        }
    }

    /// Drops the views of file tabs you closed, which stops their watchers.
    pub(super) fn forget_closed_files(&mut self) {
        let tabs = self.content.tabs();
        self.file_views
            .retain(|&id, _| tabs.contains(&Tab::File(id)));
    }

    /// The file as its tab names it: relative to the working directory.
    pub(super) fn file_place(&self, id: FileId) -> String {
        let Some(view) = self.file_views.get(&id) else {
            return String::new();
        };
        let path = view.path();
        path.strip_prefix(&self.cwd)
            .unwrap_or(path)
            .display()
            .to_string()
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
        let (id, text) = app.file_rx.recv().await.expect("read");
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
        assert!(app.file_views.is_empty());
    }
}
