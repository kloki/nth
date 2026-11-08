//! Blocking work a tool hands to another thread, stopped with the call.
//!
//! The session loop stops a call, on cancel or past its ceiling, by dropping
//! its future. A `spawn_blocking` thread does not notice that: it would go
//! on walking the tree with nobody waiting for the answer. The job here gets
//! a [`Stop`] it polls instead, set by a guard the call's future owns, so
//! dropping the future is all it takes and no task is left watching.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::task::JoinError;

/// Set once the call that started the job is gone; the job polls it.
#[derive(Debug, Clone, Default)]
pub(crate) struct Stop(Arc<AtomicBool>);

impl Stop {
    pub(crate) fn is_set(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }

    fn set(&self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Sets the stop when dropped: by the end of the call, however it ends.
struct StopOnDrop(Stop);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.set();
    }
}

/// Runs `job` on the blocking pool, setting its [`Stop`] when this future is
/// dropped. That happens after the job returned too, when nobody reads the
/// flag any more, so only a drop before then stops anything.
pub(crate) async fn run<T, F>(job: F) -> Result<T, JoinError>
where
    T: Send + 'static,
    F: FnOnce(&Stop) -> T + Send + 'static,
{
    let stop = Stop::default();
    let _guard = StopOnDrop(stop.clone());
    tokio::task::spawn_blocking(move || job(&stop)).await
}

/// Waits on the job's thread for the call to be dropped, as a test that
/// drops it does before the job goes on. A guard that never sets the stop
/// makes the test fail after a few seconds instead of hanging it, since the
/// runtime's shutdown waits for a blocking thread that would spin forever.
#[cfg(test)]
pub(crate) fn wait_for(stop: &Stop) -> bool {
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(2);
    while !stop.is_set() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    stop.is_set()
}

#[cfg(test)]
mod tests {
    use std::{sync::mpsc, time::Duration};

    use futures::FutureExt;

    use super::*;

    #[tokio::test]
    async fn dropping_the_call_stops_the_job() {
        let (ended, end) = mpsc::channel();
        let call = run(move |stop| {
            let _ = ended.send(wait_for(stop));
        });
        let mut call = Box::pin(call);
        assert!((&mut call).now_or_never().is_none(), "the job is running");
        drop(call);
        let ended = tokio::task::spawn_blocking(move || end.recv_timeout(Duration::from_secs(5)));
        assert_eq!(
            ended.await.expect("join"),
            Ok(true),
            "the job never stopped"
        );
    }

    #[tokio::test]
    async fn a_finished_job_returns_its_value() {
        let stop_seen = run(|stop| stop.is_set()).await.expect("join");
        assert!(!stop_seen);
    }
}
