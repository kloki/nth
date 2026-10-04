//! The working tree's git state on the status bar, read in the background
//! after anything that may have changed it.

use super::App;
use crate::git::{self, GitStatus};

impl App {
    /// Reads git status in the background; git is slow on a big tree.
    pub(super) fn load_git(&mut self) {
        let cwd = self.cwd.clone();
        self.git_loading
            .start_or_queue(|_| tokio::spawn(async move { git::load(&cwd).await }));
    }

    /// A failed load (no git installed, say) shows no git state rather
    /// than stopping the app.
    pub(super) fn git_loaded(&mut self, status: Result<Option<GitStatus>, String>) {
        self.git = status.ok().flatten();
        if self.git_loading.take_again() {
            self.load_git();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::app::tests::{app, rows};

    #[test]
    fn status_shows_the_branch_in_the_git_summary() {
        let mut app = app();
        app.git = Some(GitStatus {
            branch: Some("main".into()),
            ahead: 2,
            modified: 1,
            ..GitStatus::default()
        });
        app.busy_since = Some(Instant::now());
        let rows = rows(&mut app);

        assert!(rows[14].starts_with(" glm · /repo "));
        assert!(rows[14].trim_end().ends_with("git · main +2 *1"));
    }

    #[tokio::test]
    async fn a_second_git_load_waits_for_the_first() {
        let mut app = app();
        app.load_git();
        app.load_git();

        let status = app.git_loading.join().await.expect("loads");
        app.git_loaded(status);
        assert!(app.git_loading.is_running(), "the queued load starts");
        assert!(!app.git_loading.take_again(), "only once");
    }

    #[tokio::test]
    async fn dropping_the_app_aborts_the_git_load() {
        let mut app = app();
        app.load_git();
        let loading = app.git_loading.abort_handle().expect("loading");

        drop(app);
        tokio::task::yield_now().await;
        assert!(loading.is_finished());
    }
}
