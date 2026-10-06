//! The front-end's monitors: which are running and who stopped them. What
//! they print goes into the model's inbox, which the front-end owns.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{MonitorEvent, MonitorId, Registered, StoppedBy, Stream};
use crate::Inbox;

/// The front-end's monitors, shared by the monitor tools that start and
/// stop them and the front-end that shows them. The default has no
/// front-end, as in a headless run, so nobody would hear from a monitor
/// and none can start.
#[derive(Debug, Clone, Default)]
pub struct Monitors(Option<Arc<Inner>>);

#[derive(Debug)]
struct Inner {
    front_end: mpsc::Sender<MonitorEvent>,
    /// Where what the monitors print waits for the model.
    inbox: Inbox,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    next: MonitorId,
    log_dir: PathBuf,
    running: HashMap<MonitorId, Running>,
}

#[derive(Debug)]
struct Running {
    description: String,
    log: PathBuf,
    stop: CancellationToken,
    stopped_by: Option<StoppedBy>,
}

impl Monitors {
    /// Monitors that report to `front_end`, log under `log_dir`, and put
    /// what they print in `inbox`.
    pub fn new(front_end: mpsc::Sender<MonitorEvent>, log_dir: PathBuf, inbox: Inbox) -> Self {
        Self(Some(Arc::new(Inner {
            front_end,
            inbox,
            state: Mutex::new(State {
                log_dir,
                ..State::default()
            }),
        })))
    }

    /// Whether a front-end hears from monitors, so one can start.
    pub fn reaches_front_end(&self) -> bool {
        self.0.is_some()
    }

    /// Where the monitors started from now on log, such as a folder per
    /// session.
    pub fn set_log_dir(&self, log_dir: PathBuf) {
        if let Some(inner) = &self.0 {
            inner.lock().log_dir = log_dir;
        }
    }

    /// Numbers a new monitor and tells the front-end it started. `None` when
    /// there is no front-end to hear from it.
    pub async fn register(&self, description: &str, command: &str) -> Option<Registered> {
        let inner = self.0.as_ref()?;
        let registered = {
            let mut state = inner.lock();
            state.next += 1;
            let id = state.next;
            let log = state.log_dir.join(format!("{id}.log"));
            let stop = CancellationToken::new();
            state.running.insert(
                id,
                Running {
                    description: description.to_string(),
                    log: log.clone(),
                    stop: stop.clone(),
                    stopped_by: None,
                },
            );
            Registered { id, stop, log }
        };
        let started = MonitorEvent::Started {
            id: registered.id,
            description: description.to_string(),
            command: command.to_string(),
            log: registered.log.clone(),
        };
        if inner.front_end.send(started).await.is_err() {
            inner.lock().running.remove(&registered.id);
            return None;
        }
        Some(registered)
    }

    /// Stops a running monitor; `false` when there is none with that id.
    pub fn stop(&self, id: MonitorId, by: StoppedBy) -> bool {
        let Some(inner) = &self.0 else { return false };
        let mut state = inner.lock();
        let Some(running) = state.running.get_mut(&id) else {
            return false;
        };
        running.stopped_by.get_or_insert(by);
        running.stop.cancel();
        true
    }

    /// Stops every running monitor.
    pub fn stop_all(&self, by: StoppedBy) {
        let Some(inner) = &self.0 else { return };
        for running in inner.lock().running.values_mut() {
            running.stopped_by.get_or_insert(by);
            running.stop.cancel();
        }
    }

    /// Stops every running monitor for a session that was left: what they
    /// still say goes to the front-end only, never to the next session's
    /// model, since a forgotten monitor has no notice to join.
    pub fn forget_all(&self) {
        let Some(inner) = &self.0 else { return };
        for (_, running) in inner.lock().running.drain() {
            running.stop.cancel();
        }
    }

    /// Who stopped a monitor whose token was cancelled.
    pub fn stopped_by(&self, id: MonitorId) -> StoppedBy {
        self.0
            .as_ref()
            .and_then(|inner| inner.lock().running.get(&id)?.stopped_by)
            .unwrap_or(StoppedBy::Exit)
    }

    pub fn is_running(&self, id: MonitorId) -> bool {
        self.0
            .as_ref()
            .is_some_and(|inner| inner.lock().running.contains_key(&id))
    }

    pub fn running(&self) -> usize {
        self.0
            .as_ref()
            .map_or(0, |inner| inner.lock().running.len())
    }

    /// Puts a monitor's output or end in the model's inbox and passes it
    /// on to the front-end. `false` once the front-end is gone, when the
    /// monitor should stop: nobody is left to hear from it.
    pub async fn event(&self, event: MonitorEvent) -> bool {
        let Some(inner) = &self.0 else { return false };
        inner.record(&event);
        inner.front_end.send(event).await.is_ok()
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("monitor state lock poisoned")
    }
}

impl Inner {
    /// What a running monitor said goes to the inbox under its name; a
    /// forgotten one has no notice to join. Stderr is for the log only.
    fn record(&self, event: &MonitorEvent) {
        match event {
            MonitorEvent::Started { .. } => {}
            MonitorEvent::Output {
                stream: Stream::Stderr,
                ..
            } => {}
            MonitorEvent::Output { id, line, .. } => {
                if let Some((description, log)) = self.named(*id) {
                    self.inbox
                        .monitor_line(*id, &description, &log, line.clone());
                }
            }
            MonitorEvent::Ended { id, end, events } => {
                if let Some((description, log)) = self.named(*id) {
                    self.inbox
                        .monitor_ended(*id, &description, &log, *end, *events);
                }
                self.lock().running.remove(id);
            }
        }
    }

    /// The description and log of a running monitor, copied out so the
    /// inbox is never locked under the registry's lock.
    fn named(&self, id: MonitorId) -> Option<(String, PathBuf)> {
        let state = self.lock();
        let running = state.running.get(&id)?;
        Some((running.description.clone(), running.log.clone()))
    }
}

/// Where a monitor's log goes, under nth's data folder, per session.
pub fn log_dir(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir.join("monitors").join(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MonitorEnd;

    fn monitors() -> (Monitors, Inbox, mpsc::Receiver<MonitorEvent>) {
        let (tx, rx) = mpsc::channel(64);
        let inbox = Inbox::new();
        (Monitors::new(tx, "/logs".into(), inbox.clone()), inbox, rx)
    }

    async fn line(monitors: &Monitors, id: MonitorId, line: &str) {
        let output = MonitorEvent::Output {
            id,
            line: line.into(),
            stream: Stream::Stdout,
        };
        assert!(monitors.event(output).await);
    }

    #[tokio::test]
    async fn headless_has_no_monitors() {
        let monitors = Monitors::default();
        assert!(monitors.register("tail", "tail -f x").await.is_none());
        assert!(!monitors.reaches_front_end());
    }

    #[tokio::test]
    async fn register_numbers_and_announces() {
        let (monitors, _inbox, mut rx) = monitors();
        let first = monitors.register("errors", "tail -f log").await.unwrap();
        let second = monitors.register("ci", "gh run watch").await.unwrap();
        assert_eq!((first.id, second.id), (1, 2));
        assert_eq!(first.log, PathBuf::from("/logs/1.log"));
        assert_eq!(
            rx.recv().await,
            Some(MonitorEvent::Started {
                id: 1,
                description: "errors".into(),
                command: "tail -f log".into(),
                log: "/logs/1.log".into(),
            })
        );
        assert_eq!(monitors.running(), 2);
    }

    #[tokio::test]
    async fn stdout_lines_become_one_notice_and_stderr_none() {
        let (monitors, inbox, _rx) = monitors();
        let m = monitors
            .register("errors \"in\" log", "tail")
            .await
            .unwrap();
        line(&monitors, m.id, "ERROR one").await;
        let stderr = MonitorEvent::Output {
            id: m.id,
            line: "noise".into(),
            stream: Stream::Stderr,
        };
        monitors.event(stderr).await;
        line(&monitors, m.id, "ERROR two").await;

        assert!(inbox.has_notices());
        assert_eq!(
            inbox.take_notices().unwrap(),
            "<monitor id=\"1\" description=\"errors &quot;in&quot; log\" log=\"/logs/1.log\">\n\
             ERROR one\nERROR two\n</monitor>"
        );
        assert_eq!(inbox.take_notices(), None, "taken");
    }

    #[tokio::test]
    async fn ending_adds_an_end_notice_and_stops_running() {
        let (monitors, inbox, _rx) = monitors();
        let m = monitors.register("ci", "watch").await.unwrap();
        line(&monitors, m.id, "build ok").await;
        let ended = MonitorEvent::Ended {
            id: m.id,
            end: MonitorEnd::Exited(Some(1)),
            events: 1,
        };
        monitors.event(ended).await;

        assert_eq!(monitors.running(), 0);
        assert_eq!(
            inbox.take_notices().unwrap(),
            "<monitor id=\"1\" description=\"ci\" log=\"/logs/1.log\">\nbuild ok\n</monitor>\n\
             <monitor id=\"1\" description=\"ci\" log=\"/logs/1.log\" ended=\"exited with code 1\" events=\"1\"/>"
        );
    }

    #[tokio::test]
    async fn stop_remembers_who_stopped_it() {
        let (monitors, _inbox, _rx) = monitors();
        let m = monitors.register("ci", "watch").await.unwrap();
        assert!(!monitors.stop(9, StoppedBy::Model), "no such monitor");
        assert!(monitors.stop(m.id, StoppedBy::User));
        assert!(m.stop.is_cancelled());
        monitors.stop_all(StoppedBy::Exit);
        assert_eq!(
            monitors.stopped_by(m.id),
            StoppedBy::User,
            "the first stop counts"
        );
    }

    #[tokio::test]
    async fn forgotten_monitors_say_nothing_more_to_the_model() {
        let (monitors, inbox, mut rx) = monitors();
        let m = monitors.register("ci", "watch").await.unwrap();
        line(&monitors, m.id, "before").await;
        monitors.forget_all();
        inbox.clear();
        assert!(m.stop.is_cancelled());
        assert_eq!(monitors.running(), 0);
        let ended = MonitorEvent::Ended {
            id: m.id,
            end: MonitorEnd::Stopped(StoppedBy::Exit),
            events: 1,
        };
        assert!(monitors.event(ended.clone()).await, "the tab still hears");
        assert_eq!(inbox.take_notices(), None);
        while let Ok(event) = rx.try_recv() {
            if matches!(event, MonitorEvent::Ended { .. }) {
                return;
            }
        }
        panic!("the front-end never heard it ended");
    }
}
