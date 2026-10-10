//! The working tree's git state on the status bar, read in the background
//! after anything that may have changed it: the status itself, and the
//! branch's pull request.

use super::App;
use crate::git::{self, GitStatus, Pr};

impl App {
    /// Reads git status in the background; git is slow on a big tree.
    pub(super) fn load_git(&mut self) {
        let cwd = self.cwd.clone();
        self.git_loading
            .start_or_queue(|_| tokio::spawn(async move { git::load(&cwd).await }));
    }

    /// Reads the branch's pull request in the background; the forge's
    /// tool is slow, and may be missing or not logged in. Asked at
    /// start-up, at the end of a turn and on a move, not after every
    /// write: it goes over the network, and an edit never changes it.
    pub(super) fn load_pr(&mut self) {
        let cwd = self.cwd.clone();
        self.pr_loading
            .start_or_queue(|_| tokio::spawn(async move { git::load_pr(&cwd).await }));
    }

    /// A failed load (no git installed, say) shows no git state rather
    /// than stopping the app.
    pub(super) fn git_loaded(&mut self, status: Result<Option<GitStatus>, String>) {
        self.git = status.ok().flatten();
        if self.git_loading.take_again() {
            self.load_git();
        }
    }

    /// A load that found no pull request, because the tool is missing or
    /// the branch has none, shows no link rather than stopping the app.
    pub(super) fn pr_loaded(&mut self, pr: Option<Pr>) {
        self.pr = pr;
        if self.pr_loading.take_again() {
            self.load_pr();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::{
        app::tests::{app, rows},
        git::State,
    };

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

    #[test]
    fn the_branchs_pull_request_shows_after_the_branch_as_a_link() {
        let mut app = app();
        app.git = Some(GitStatus {
            branch: Some("main".into()),
            ahead: 2,
            modified: 1,
            ..GitStatus::default()
        });
        app.pr = Some(Pr {
            number: 123,
            url: "https://x.y/repo/pull/123".into(),
            state: State::Merged,
        });
        let rows = rows(&mut app);

        assert!(rows[14].trim_end().ends_with("git · main #123 +2 *1"));
        // The link covers exactly its number, so a click lands on it alone.
        let area = app.areas.pr_link.expect("the link shows");
        let link: String = rows[usize::from(area.y)]
            .chars()
            .skip(usize::from(area.x))
            .take(usize::from(area.width))
            .collect();
        assert_eq!(link, "#123");
    }

    #[test]
    fn a_link_a_narrow_line_cuts_off_is_not_clickable() {
        let mut app = app();
        app.git = Some(GitStatus {
            branch: Some("a-very-long-branch-name".into()),
            ..GitStatus::default()
        });
        app.pr = Some(Pr {
            number: 123,
            url: "https://x.y/repo/pull/123".into(),
            state: State::Open,
        });
        let rows = rows(&mut app);

        assert!(!rows[14].contains("#123"), "the right side is cut first");
        assert_eq!(app.areas.pr_link, None);
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
    async fn a_second_pr_load_waits_for_the_first() {
        let mut app = app();
        app.load_pr();
        app.load_pr();

        let pr = app.pr_loading.join().await.expect("loads");
        app.pr_loaded(pr);
        assert!(app.pr_loading.is_running(), "the queued load starts");
        assert!(!app.pr_loading.take_again(), "only once");
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

    #[tokio::test]
    async fn dropping_the_app_aborts_the_pr_load() {
        let mut app = app();
        app.load_pr();
        let loading = app.pr_loading.abort_handle().expect("loading");

        drop(app);
        tokio::task::yield_now().await;
        assert!(loading.is_finished());
    }
}
