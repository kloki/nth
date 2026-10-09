//! The files `@` mentions complete to, walked in the background and again
//! after every turn, since the model may have added some.

use super::{App, completion::Completion};
use crate::mention;

impl App {
    /// Lists the files off the runtime; a big tree takes a while to walk.
    /// The walk checks its token between entries, since a blocking task
    /// can't be aborted and quitting shouldn't wait for it. A directory
    /// added with `/add-dir` walks with the working one, its files spelled
    /// absolutely so the tools take them as they are.
    pub(super) fn index_files(&mut self) {
        let root = self.cwd.clone();
        let extra = self.extra_dirs.clone();
        self.indexing.start_or_queue(|cancel| {
            tokio::task::spawn_blocking(move || mention::walk(&root, &extra, &cancel))
        });
    }

    pub(super) fn indexed(&mut self, files: Vec<String>) {
        self.files = files;
        if self.indexing.take_again() {
            self.index_files();
        }
        if matches!(self.completion, Some(Completion::Mention { .. })) {
            self.refresh_completion();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::app::tests::app;

    #[tokio::test]
    async fn a_second_walk_waits_for_the_first() {
        let mut app = app();
        app.index_files();
        app.index_files();

        let files = app.indexing.join().await.expect("walks");
        app.indexed(files);
        assert!(app.indexing.is_running(), "the queued walk starts");
        assert!(!app.indexing.take_again(), "only once");
    }

    #[tokio::test]
    async fn dropping_the_app_cancels_the_walk() {
        let mut app = app();
        app.index_files();
        let cancel = app.indexing.token().expect("walking");

        drop(app);
        assert!(cancel.is_cancelled());
    }
}
