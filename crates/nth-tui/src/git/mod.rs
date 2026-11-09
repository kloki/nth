//! The git state of the working directory, for the status bar: the branch,
//! the branch's pull request, and a starship-style summary of what is
//! changed.

mod pr;
mod status;
mod view;

use std::{path::PathBuf, time::Duration};

/// The state the forge reports, for tests that build a `Pr`.
#[cfg(test)]
pub(crate) use pr::State;
pub use pr::{Pr, load as load_pr};
pub use status::{GitStatus, load};
use tokio::task::JoinError;
pub use view::summary;

use crate::job::Job;

/// The longest git or a forge's tool may take to answer. A tool waiting
/// on the network, or on a lock, would otherwise keep its load running for
/// good, and every later change only marks it to run again; ten seconds is
/// well above what a working forge takes.
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(10);

/// What the status bar shows of git, and the two loads that refresh it.
#[derive(Default)]
pub struct GitBar {
    /// `None` outside a repository or until the first load lands.
    pub status: Option<GitStatus>,
    /// The branch's pull request, as gh or tea found it; `None` until a
    /// load lands, and whenever the branch has none.
    pub pr: Option<Pr>,
    /// Queued again when the tree may have changed mid-load, so a second
    /// load never races the first.
    pub(crate) loading: Job<Result<Option<GitStatus>, String>>,
    pub(crate) pr_loading: Job<Option<Pr>>,
}

/// A load that ended, with what it found or how its task failed.
pub enum Loaded {
    Status(Result<Result<Option<GitStatus>, String>, JoinError>),
    Pr(Result<Option<Pr>, JoinError>),
}

impl GitBar {
    /// Reads git status in `cwd` in the background; git is slow on a big
    /// tree.
    pub fn load(&mut self, cwd: PathBuf) {
        self.loading
            .start_or_queue(|_| tokio::spawn(async move { load(&cwd).await }));
    }

    /// Reads the branch's pull request in the background; the forge's tool
    /// goes over the network, and may be missing or not logged in.
    pub fn load_pr(&mut self, cwd: PathBuf) {
        self.pr_loading
            .start_or_queue(|_| tokio::spawn(async move { load_pr(&cwd).await }));
    }

    /// Resolves when either load ends. Never resolves while both are idle,
    /// so a `select!` arm needs no guard; cancel-safe, as both joins are.
    pub async fn next(&mut self) -> Loaded {
        tokio::select! {
            status = self.loading.join() => Loaded::Status(status),
            pr = self.pr_loading.join() => Loaded::Pr(pr),
        }
    }
}
