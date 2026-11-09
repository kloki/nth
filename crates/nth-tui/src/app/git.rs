//! The working tree's git state on the status bar, read in the background
//! after anything that may have changed it: the status itself, and the
//! branch's pull request.

use super::{App, mouse};
use crate::git::{GitStatus, Loaded, Pr};

impl App {
    /// Reads git status in the background; git is slow on a big tree.
    pub(super) fn load_git(&mut self) {
        self.git.load(self.cwd.clone());
    }

    /// Reads the branch's pull request in the background; the forge's
    /// tool is slow, and may be missing or not logged in. Asked at
    /// start-up, at the end of a turn and on a move, not after every
    /// write: it goes over the network, and an edit never changes it.
    pub(super) fn load_pr(&mut self) {
        self.git.load_pr(self.cwd.clone());
    }

    /// Takes in a load that ended. One whose task panicked says so and
    /// leaves what was shown; either way a load asked for while it ran
    /// starts now, so a timed-out or failed one is not the last word.
    pub(super) fn git_finished(&mut self, loaded: Loaded) {
        match loaded {
            Loaded::Status(Ok(status)) => self.git_loaded(status),
            Loaded::Status(Err(e)) => {
                self.job_failed("reading git status", e);
                if self.git.loading.take_again() {
                    self.load_git();
                }
            }
            Loaded::Pr(Ok(pr)) => self.pr_loaded(pr),
            Loaded::Pr(Err(e)) => {
                self.job_failed("reading the pull request", e);
                if self.git.pr_loading.take_again() {
                    self.load_pr();
                }
            }
        }
    }

    /// A failed load (no git installed, or one that timed out) shows no
    /// git state rather than stopping the app.
    fn git_loaded(&mut self, status: Result<Option<GitStatus>, String>) {
        self.git.status = status.ok().flatten();
        if self.git.loading.take_again() {
            self.load_git();
        }
    }

    /// A load that found no pull request, because the tool is missing,
    /// timed out or the branch has none, shows no link rather than
    /// stopping the app.
    pub(super) fn pr_loaded(&mut self, pr: Option<Pr>) {
        self.areas.forget(&mouse::Target::PullRequest);
        self.git.pr = pr;
        if self.git.pr_loading.take_again() {
            self.load_pr();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::{
        app::tests::{rows, test_app},
        git::{self, State},
    };

    #[test]
    fn status_shows_the_branch_in_the_git_summary() {
        let mut app = test_app();
        app.git.status = Some(GitStatus {
            branch: Some("main".into()),
            ahead: 2,
            modified: 1,
            ..GitStatus::default()
        });
        app.busy_since = Some(Instant::now());
        let rows = rows(&mut app);

        assert!(crate::app::tests::status_row(&rows, 0).starts_with(" /repo "));
        assert!(
            crate::app::tests::status_row(&rows, 0)
                .trim_end()
                .ends_with("git · main +2 *1")
        );
    }

    #[test]
    fn the_branchs_pull_request_shows_after_the_branch_as_a_link() {
        let mut app = test_app();
        app.git.status = Some(GitStatus {
            branch: Some("main".into()),
            ahead: 2,
            modified: 1,
            ..GitStatus::default()
        });
        app.git.pr = Some(Pr {
            number: 123,
            url: "https://x.y/repo/pull/123".into(),
            state: State::Merged,
        });
        let rows = rows(&mut app);

        assert!(
            crate::app::tests::status_row(&rows, 0)
                .trim_end()
                .ends_with("git · main #123 +2 *1")
        );
        // The link covers exactly its number, so a click lands on it alone.
        let area = app
            .areas
            .find(&mouse::Target::PullRequest)
            .expect("the link shows");
        let link: String = rows[usize::from(area.y)]
            .chars()
            .skip(usize::from(area.x))
            .take(usize::from(area.width))
            .collect();
        assert_eq!(link, "#123");
    }

    #[test]
    fn a_link_a_narrow_line_cuts_off_is_not_clickable() {
        let mut app = test_app();
        app.git.status = Some(GitStatus {
            branch: Some("a-very-long-branch-name".into()),
            ..GitStatus::default()
        });
        app.git.pr = Some(Pr {
            number: 123,
            url: "https://x.y/repo/pull/123".into(),
            state: State::Open,
        });
        let rows = rows(&mut app);

        assert!(
            !crate::app::tests::status_row(&rows, 0).contains("#123"),
            "the right side is cut first"
        );
        assert_eq!(app.areas.find(&mouse::Target::PullRequest), None);
    }

    #[tokio::test]
    async fn a_second_git_load_waits_for_the_first() {
        let mut app = test_app();
        app.load_git();
        app.load_git();

        let loaded = app.git.next().await;
        app.git_finished(loaded);
        assert!(app.git.loading.is_running(), "the queued load starts");
        assert!(!app.git.loading.take_again(), "only once");
    }

    #[tokio::test]
    async fn a_second_pr_load_waits_for_the_first() {
        let mut app = test_app();
        app.load_pr();
        app.load_pr();

        let loaded = app.git.next().await;
        app.git_finished(loaded);
        assert!(app.git.pr_loading.is_running(), "the queued load starts");
        assert!(!app.git.pr_loading.take_again(), "only once");
    }

    #[tokio::test]
    async fn a_failed_git_load_reruns_on_the_next_change() {
        let mut app = test_app();
        let timeout = format!("git took longer than {} s", git::LOAD_TIMEOUT.as_secs());
        app.git
            .loading
            .start(|_| tokio::spawn(async move { Err(timeout) }));
        // The tree changed while the load hung.
        app.load_git();

        let loaded = app.git.next().await;
        app.git_finished(loaded);
        assert_eq!(app.git.status, None, "the timed-out load shows nothing");
        assert!(app.git.loading.is_running(), "the change loads again");
    }

    #[tokio::test]
    async fn dropping_the_app_aborts_the_git_load() {
        let mut app = test_app();
        app.load_git();
        let loading = app.git.loading.abort_handle().expect("loading");

        drop(app);
        tokio::task::yield_now().await;
        assert!(loading.is_finished());
    }

    #[tokio::test]
    async fn dropping_the_app_aborts_the_pr_load() {
        let mut app = test_app();
        app.load_pr();
        let loading = app.git.pr_loading.abort_handle().expect("loading");

        drop(app);
        tokio::task::yield_now().await;
        assert!(loading.is_finished());
    }
}
