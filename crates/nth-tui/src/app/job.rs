//! A background task the app's loop waits on: a turn, a file walk, a git
//! load or a listing.

use nth_session::CancellationToken;
use tokio::task::{JoinError, JoinHandle};

/// At most one task of a kind, which stops when the job is dropped, so
/// quitting never leaves the agent running tools or a walk going. Dropping
/// cancels the task's token, for blocking work that can't be aborted, and
/// aborts it.
pub(super) struct Job<T> {
    running: Option<(JoinHandle<T>, CancellationToken)>,
    /// Asked for again while running; the handler starts it once more.
    again: bool,
}

impl<T> Default for Job<T> {
    fn default() -> Self {
        Self {
            running: None,
            again: false,
        }
    }
}

impl<T> Job<T> {
    pub(super) fn is_running(&self) -> bool {
        self.running.is_some()
    }

    /// Starts the task `spawn` makes, stopping the one running.
    pub(super) fn start(&mut self, spawn: impl FnOnce(CancellationToken) -> JoinHandle<T>) {
        self.stop();
        let cancel = CancellationToken::new();
        self.running = Some((spawn(cancel.clone()), cancel));
    }

    /// Starts the task `spawn` makes, or while one runs, marks the job to
    /// run again rather than racing it.
    pub(super) fn start_or_queue(
        &mut self,
        spawn: impl FnOnce(CancellationToken) -> JoinHandle<T>,
    ) {
        if self.is_running() {
            self.again = true;
        } else {
            self.start(spawn);
        }
    }

    /// Asks the running task to stop on its own, so it can hand back what
    /// it holds.
    pub(super) fn cancel(&self) {
        if let Some((_, cancel)) = &self.running {
            cancel.cancel();
        }
    }

    /// Whether the job was asked for again while it ran; clears the mark.
    pub(super) fn take_again(&mut self) -> bool {
        std::mem::take(&mut self.again)
    }

    /// Resolves when the task ends, leaving the job idle. Never resolves
    /// while idle, so a `select!` arm needs no guard. Cancel-safe: dropped
    /// unfinished, the task keeps running.
    pub(super) async fn join(&mut self) -> Result<T, JoinError> {
        let Some((handle, _)) = &mut self.running else {
            return std::future::pending().await;
        };
        let result = handle.await;
        self.running = None;
        result
    }

    fn stop(&mut self) {
        if let Some((handle, cancel)) = self.running.take() {
            cancel.cancel();
            handle.abort();
        }
    }

    /// The running task's handle, leaving the job idle without stopping it.
    #[cfg(test)]
    pub(super) fn take(&mut self) -> Option<JoinHandle<T>> {
        self.running.take().map(|(handle, _)| handle)
    }

    #[cfg(test)]
    pub(super) fn token(&self) -> Option<CancellationToken> {
        self.running.as_ref().map(|(_, cancel)| cancel.clone())
    }

    #[cfg(test)]
    pub(super) fn abort_handle(&self) -> Option<tokio::task::AbortHandle> {
        self.running
            .as_ref()
            .map(|(handle, _)| handle.abort_handle())
    }
}

impl<T> Drop for Job<T> {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn forever(_: CancellationToken) -> JoinHandle<()> {
        tokio::spawn(std::future::pending())
    }

    #[tokio::test]
    async fn an_idle_job_never_ends() {
        let mut job = Job::<()>::default();
        let joined = tokio::time::timeout(Duration::from_millis(10), job.join()).await;
        assert!(joined.is_err());
    }

    #[tokio::test]
    async fn joining_leaves_the_job_idle() {
        let mut job = Job::default();
        job.start(|_| tokio::spawn(async { 7 }));
        assert_eq!(job.join().await.expect("finishes"), 7);
        assert!(!job.is_running());
    }

    #[tokio::test]
    async fn a_second_start_while_running_is_queued() {
        let mut job = Job::default();
        job.start_or_queue(forever);
        let first = job.abort_handle().expect("running");
        job.start_or_queue(|_| panic!("must not start"));

        assert!(job.take_again());
        assert!(!job.take_again(), "cleared");
        assert!(!first.is_finished(), "the first keeps running");
    }

    #[tokio::test]
    async fn starting_stops_the_running_task() {
        let mut job = Job::default();
        job.start(forever);
        let first = job.abort_handle().expect("running");
        let token = job.token().expect("running");

        job.start(forever);
        tokio::task::yield_now().await;
        assert!(first.is_finished());
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn dropping_cancels_and_aborts() {
        let mut job = Job::default();
        job.start(forever);
        let handle = job.abort_handle().expect("running");
        let token = job.token().expect("running");

        drop(job);
        tokio::task::yield_now().await;
        assert!(handle.is_finished());
        assert!(token.is_cancelled());
    }
}
