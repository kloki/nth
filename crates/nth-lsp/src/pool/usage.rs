//! What each server costs and who uses it: the usage watch, and the sampler
//! that reads the servers' CPU and memory while anyone watches.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Weak, atomic::Ordering},
    time::{Duration, SystemTime},
};

use sysinfo::{Pid, ProcessesToUpdate, System};
use tokio::{sync::watch, time::MissedTickBehavior};

use super::{Entry, Inner, Key, Lsp, ServerState};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Caller {
    pub session: String,
    pub is_subagent: bool,
    pub agent: Option<String>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct CallerUsage {
    pub caller: Caller,
    pub active: usize,
    pub last_used: SystemTime,
}
/// Shared process metrics; CPU is percent of one core, memory is RSS bytes.
#[derive(Debug, Clone, PartialEq)]
pub struct ProcessUsage {
    pub id: String,
    pub root: PathBuf,
    pub state: ServerState,
    pub pid: Option<u32>,
    pub active: usize,
    pub last_used: SystemTime,
    pub cpu_percent: Option<f32>,
    pub memory_bytes: Option<u64>,
    pub callers: Vec<CallerUsage>,
}

impl ProcessUsage {
    pub(super) fn starting(id: String, root: PathBuf) -> Self {
        Self {
            id,
            root,
            state: ServerState::Starting,
            pid: None,
            active: 0,
            last_used: SystemTime::now(),
            cpu_percent: None,
            memory_bytes: None,
            callers: Vec::new(),
        }
    }

    pub(super) fn enter(&mut self, caller: Option<&Caller>) {
        self.active += 1;
        self.last_used = SystemTime::now();
        let Some(caller) = caller else { return };
        if let Some(usage) = self.callers.iter_mut().find(|u| &u.caller == caller) {
            usage.active += 1;
            usage.last_used = SystemTime::now();
        } else {
            self.callers.push(CallerUsage {
                caller: caller.clone(),
                active: 1,
                last_used: SystemTime::now(),
            });
        }
    }

    pub(super) fn leave(&mut self, caller: Option<&Caller>) {
        self.active -= 1;
        self.last_used = SystemTime::now();
        if let Some(caller) = caller
            && let Some(usage) = self.callers.iter_mut().find(|u| &u.caller == caller)
        {
            usage.active -= 1;
            usage.last_used = SystemTime::now();
        }
    }
}

/// Every entry's usage, in a stable order. Taken under the clients lock and
/// published after it is released, since publishing waits for any reader
/// of the watch and nothing else should wait behind that.
pub(super) fn snapshot(clients: &HashMap<Key, Entry>) -> Vec<ProcessUsage> {
    let mut usage: Vec<_> = clients.values().map(|e| e.usage.clone()).collect();
    usage.sort_by(|a, b| (&a.id, &a.root).cmp(&(&b.id, &b.root)));
    usage
}

impl Inner {
    /// Sends `usage` from [`snapshot`]; never call it holding the clients
    /// lock. Two racing callers may land an older snapshot last: the watch
    /// is a display, corrected by the next change or sample.
    pub(super) fn publish(&self, usage: Vec<ProcessUsage>) {
        self.usage.send_if_modified(|old| {
            if *old == usage {
                false
            } else {
                *old = usage;
                true
            }
        });
    }

    /// Clears `sampling` when nobody watches, saying whether the sampler
    /// should stop.
    fn stop_sampling_if_unwatched(&self) -> bool {
        if self.usage.receiver_count() > 0 {
            return false;
        }
        self.sampling.store(false, Ordering::Release);
        // A subscriber may have arrived before the flag was cleared; if it
        // already started a sampler of its own, this one leaves.
        self.usage.receiver_count() == 0 || self.sampling.swap(true, Ordering::AcqRel)
    }

    fn pids(&self) -> Vec<u32> {
        let clients = self.clients.lock().expect("lsp client map lock poisoned");
        clients.values().filter_map(|e| e.usage.pid).collect()
    }

    /// Writes what `system` read into every entry and publishes it.
    /// `identities` is each pid's start time, so a pid the OS gave to
    /// another process is not taken for the server.
    fn record_samples(&self, system: &System, identities: &mut HashMap<u32, u64>) {
        let usage = {
            let mut clients = self.clients.lock().expect("lsp client map lock poisoned");
            for entry in clients.values_mut() {
                let Some(pid) = entry.usage.pid else { continue };
                let process = system.process(Pid::from_u32(pid));
                if let Some(process) = process {
                    let identity = process.start_time();
                    let previous = identities.get(&pid).copied();
                    if previous.is_none() || previous == Some(identity) {
                        identities.insert(pid, identity);
                        entry.usage.memory_bytes = Some(process.memory());
                        entry.usage.cpu_percent = if previous == Some(identity) {
                            Some(process.cpu_usage())
                        } else {
                            None
                        };
                    } else {
                        entry.usage.memory_bytes = None;
                        entry.usage.cpu_percent = None;
                    }
                } else {
                    entry.usage.memory_bytes = None;
                    entry.usage.cpu_percent = None;
                }
            }
            snapshot(&clients)
        };
        self.publish(usage);
    }
}

impl Lsp {
    /// Subscribe to process and caller usage. Sampling stops after subscribers leave.
    /// Clone the snapshot and release the watch borrow before awaiting, cancelling
    /// a touch, or otherwise calling into the pool: publishing needs its write lock.
    pub fn usage(&self) -> watch::Receiver<Vec<ProcessUsage>> {
        let receiver = self.inner.usage.subscribe();
        // Held from the swap to the store, so two subscribers store their
        // samplers in the order they started them: otherwise the later
        // could find the earlier's stale handle replacing its live one.
        let mut sampler = self
            .inner
            .sampler
            .lock()
            .expect("lsp sampler lock poisoned");
        if !self.inner.sampling.swap(true, Ordering::AcqRel) {
            let task = tokio::spawn(sample_usage(Arc::downgrade(&self.inner)));
            // A sampler this replaces has cleared `sampling` and is leaving.
            if let Some(old) = sampler.replace(task) {
                old.abort();
            }
        }
        drop(sampler);
        receiver
    }
}

/// Samples the servers' CPU and memory once a second until nobody watches
/// or the pool goes. Only reading `/proc` runs on a blocking thread, and
/// only for as long as that takes.
async fn sample_usage(inner: Weak<Inner>) {
    let mut system = System::new();
    let mut identities = HashMap::<u32, u64>::new();
    let mut ticks = tokio::time::interval(Duration::from_secs(1));
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let pids = {
            let Some(pool) = inner.upgrade() else { return };
            if pool.stop_sampling_if_unwatched() {
                return;
            }
            pool.pids()
        };
        identities.retain(|pid, _| pids.contains(pid));
        if !pids.is_empty() {
            let ids: Vec<_> = pids.iter().map(|id| Pid::from_u32(*id)).collect();
            system = tokio::task::spawn_blocking(move || {
                system.refresh_processes(ProcessesToUpdate::Some(&ids), true);
                system
            })
            .await
            // A refresh that panicked loses the CPU baseline, nothing more.
            .unwrap_or_else(|_| System::new());
        }
        let Some(pool) = inner.upgrade() else { return };
        pool.record_samples(&system, &mut identities);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::pool::fake::{scripted, scripted_idling};

    #[tokio::test]
    async fn callers_share_process_and_cancelled_checks_become_idle() {
        let (_tmp, file, lsp) = scripted_idling(&[], Some(Duration::from_millis(300)));
        let mut usage = lsp.usage();
        let caller = |session: &str| Caller {
            session: session.into(),
            is_subagent: true,
            agent: Some("explore".into()),
        };
        let first_lsp = lsp.clone();
        let first_file = file.clone();
        let first_caller = caller("one");
        let first = tokio::spawn(async move {
            first_lsp
                .touch_attributed(&first_file, true, first_caller)
                .await
        });
        // `wait_for` returns a watch read guard. Release it before aborting:
        // cancellation drops Busy, which synchronously publishes usage.
        drop(
            tokio::time::timeout(
                Duration::from_secs(5),
                usage.wait_for(|u| u.first().is_some_and(|p| p.active == 1)),
            )
            .await
            .unwrap()
            .unwrap(),
        );
        first.abort();
        let _ = first.await;
        assert_eq!(usage.borrow()[0].callers[0].active, 0);
        tokio::time::timeout(
            Duration::from_secs(10),
            lsp.touch_attributed(&file, true, caller("two")),
        )
        .await
        .expect("second caller's check completed");
        let snapshot = usage.borrow().clone();
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot[0].pid.is_some());
        assert_eq!(snapshot[0].active, 0);
        assert_eq!(snapshot[0].callers.len(), 2);
        assert!(snapshot[0].callers.iter().all(|c| c.active == 0));
        tokio::time::timeout(Duration::from_secs(5), usage.wait_for(|u| u.is_empty()))
            .await
            .unwrap()
            .unwrap();
    }

    /// A watcher holding the usage borrow keeps a touch from publishing,
    /// but not the clients lock, which every other touch and the reaper
    /// need: the waiting touch holds nothing while it waits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn publishing_usage_does_not_hold_the_clients_lock() {
        let (_tmp, file, lsp) = scripted(&[]);
        let usage = lsp.usage();
        // Started first, so the touch below only counts itself in.
        assert_eq!(lsp.touch(&file, true).await[&file][0].message, "boom");

        let held = usage.borrow();
        let touch = tokio::spawn({
            let (lsp, file) = (lsp.clone(), file.clone());
            async move { lsp.touch(&file, false).await }
        });
        // The touch counts itself in under the lock and then publishes,
        // which waits for `held`. Seeing its count with the lock free means
        // it got there without keeping the lock.
        let counted = (0..500).any(|_| {
            let free = match lsp.inner.clients.try_lock() {
                Ok(clients) => clients.values().any(|entry| entry.busy == 1),
                Err(_) => false,
            };
            if !free {
                std::thread::sleep(Duration::from_millis(10));
            }
            free
        });
        drop(held);
        assert!(counted, "the waiting touch held the clients lock");
        let diagnostics = tokio::time::timeout(Duration::from_secs(10), touch)
            .await
            .expect("the touch finished once the borrow went")
            .unwrap();
        assert_eq!(diagnostics[&file][0].message, "boom");
    }

    #[tokio::test(start_paused = true)]
    async fn the_sampler_stops_with_its_last_subscriber() {
        // No servers, so sampling reads nothing and the clock can stay paused.
        let lsp = Lsp::with_servers(Vec::new(), None);
        let usage = lsp.usage();
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(lsp.inner.sampling.load(Ordering::Acquire));
        assert!(sampler_running(&lsp));

        drop(usage);
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!lsp.inner.sampling.load(Ordering::Acquire));
        assert!(!sampler_running(&lsp));

        // A new subscriber starts it again.
        let _usage = lsp.usage();
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(sampler_running(&lsp));
    }

    #[tokio::test(start_paused = true)]
    async fn the_sampler_stops_with_the_pool() {
        let lsp = Lsp::with_servers(Vec::new(), None);
        let _usage = lsp.usage();
        let task = lsp
            .inner
            .sampler
            .lock()
            .unwrap()
            .as_ref()
            .map(|task| task.abort_handle())
            .unwrap();
        // Between ticks, as it is nearly all the time.
        tokio::time::sleep(Duration::from_millis(2500)).await;
        assert!(!task.is_finished());

        drop(lsp);
        // Well before the next tick, which would also have ended it.
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(task.is_finished());
    }

    fn sampler_running(lsp: &Lsp) -> bool {
        let sampler = lsp.inner.sampler.lock().unwrap();
        sampler.as_ref().is_some_and(|task| !task.is_finished())
    }
}
