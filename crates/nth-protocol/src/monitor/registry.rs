//! The front-end's monitors: which are running, who stopped them, and what
//! the model has not heard from them yet.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{MonitorEnd, MonitorEvent, MonitorId, Registered, StoppedBy, Stream, notice};

/// The front-end's monitors, shared by the monitor tools, the agent loop
/// that hands their notices to the model, and the front-end that shows
/// them. The default has no front-end, as in a headless run, so nobody
/// would hear from a monitor and none can start.
#[derive(Debug, Clone, Default)]
pub struct Monitors(Option<Arc<Inner>>);

#[derive(Debug)]
struct Inner {
    front_end: mpsc::Sender<MonitorEvent>,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    next: MonitorId,
    log_dir: PathBuf,
    running: HashMap<MonitorId, Running>,
    /// What the model has not seen yet, in the order it happened.
    pending: Vec<Pending>,
}

#[derive(Debug)]
struct Running {
    description: String,
    log: PathBuf,
    stop: CancellationToken,
    stopped_by: Option<StoppedBy>,
}

#[derive(Debug)]
struct Pending {
    id: MonitorId,
    description: String,
    log: PathBuf,
    lines: Vec<String>,
    /// Lines past [`notice::NOTICE_LINES`].
    more: usize,
    ended: Option<(MonitorEnd, usize)>,
}

impl Monitors {
    /// Monitors that report to `front_end` and log under `log_dir`.
    pub fn new(front_end: mpsc::Sender<MonitorEvent>, log_dir: PathBuf) -> Self {
        Self(Some(Arc::new(Inner {
            front_end,
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
    /// model.
    pub fn forget_all(&self) {
        let Some(inner) = &self.0 else { return };
        let mut state = inner.lock();
        for (_, running) in state.running.drain() {
            running.stop.cancel();
        }
        state.pending.clear();
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

    /// Records a monitor's output or end for the model and passes it on to
    /// the front-end. `false` once the front-end is gone, when the monitor
    /// should stop: nobody is left to hear from it.
    pub async fn event(&self, event: MonitorEvent) -> bool {
        let Some(inner) = &self.0 else { return false };
        inner.lock().record(&event);
        inner.front_end.send(event).await.is_ok()
    }

    /// Whether the model has notices waiting.
    pub fn has_notices(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|inner| !inner.lock().pending.is_empty())
    }

    /// The notices the model has not seen, as one message, and forgets them.
    pub fn take_notices(&self) -> Option<String> {
        let inner = self.0.as_ref()?;
        let pending = std::mem::take(&mut inner.lock().pending);
        if pending.is_empty() {
            return None;
        }
        let notices: Vec<String> = pending.iter().map(Pending::notice).collect();
        Some(notices.join("\n"))
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl State {
    fn record(&mut self, event: &MonitorEvent) {
        match event {
            MonitorEvent::Started { .. } => {}
            MonitorEvent::Output {
                stream: Stream::Stderr,
                ..
            } => {}
            MonitorEvent::Output { id, line, .. } => {
                let Some(pending) = self.pending_for(*id) else {
                    return;
                };
                if pending.lines.len() < notice::NOTICE_LINES {
                    pending.lines.push(line.clone());
                } else {
                    pending.more += 1;
                }
            }
            MonitorEvent::Ended { id, end, events } => {
                if let Some(pending) = self.pending_for(*id) {
                    pending.ended = Some((*end, *events));
                }
                self.running.remove(id);
            }
        }
    }

    /// The monitor's notice still being filled: the last one, unless it has
    /// already ended.
    fn pending_for(&mut self, id: MonitorId) -> Option<&mut Pending> {
        let open = self
            .pending
            .iter()
            .rposition(|p| p.id == id && p.ended.is_none());
        let index = match open {
            Some(index) => index,
            None => {
                let running = self.running.get(&id)?;
                self.pending.push(Pending {
                    id,
                    description: running.description.clone(),
                    log: running.log.clone(),
                    lines: Vec::new(),
                    more: 0,
                    ended: None,
                });
                self.pending.len() - 1
            }
        };
        Some(&mut self.pending[index])
    }
}

impl Pending {
    fn notice(&self) -> String {
        notice::render(
            self.id,
            &self.description,
            &self.log,
            &self.lines,
            self.more,
            self.ended,
        )
    }
}

/// Where a monitor's log goes, under nth's data folder, per session.
pub fn log_dir(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir.join("monitors").join(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::NOTICE_LINES;

    fn monitors() -> (Monitors, mpsc::Receiver<MonitorEvent>) {
        let (tx, rx) = mpsc::channel(64);
        (Monitors::new(tx, "/logs".into()), rx)
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
        assert_eq!(monitors.take_notices(), None);
    }

    #[tokio::test]
    async fn register_numbers_and_announces() {
        let (monitors, mut rx) = monitors();
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
        let (monitors, _rx) = monitors();
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

        assert!(monitors.has_notices());
        assert_eq!(
            monitors.take_notices().unwrap(),
            "<monitor id=\"1\" description=\"errors &quot;in&quot; log\" log=\"/logs/1.log\">\n\
             ERROR one\nERROR two\n</monitor>"
        );
        assert_eq!(monitors.take_notices(), None, "taken");
    }

    #[tokio::test]
    async fn a_notice_caps_its_lines() {
        let (monitors, _rx) = monitors();
        let m = monitors.register("spam", "yes").await.unwrap();
        for i in 0..NOTICE_LINES + 3 {
            line(&monitors, m.id, &i.to_string()).await;
        }
        let notice = monitors.take_notices().unwrap();
        assert!(notice.contains("\n49\n… 3 more lines in the log\n</monitor>"));
    }

    #[tokio::test]
    async fn ending_adds_an_end_notice_and_stops_running() {
        let (monitors, _rx) = monitors();
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
            monitors.take_notices().unwrap(),
            "<monitor id=\"1\" description=\"ci\" log=\"/logs/1.log\">\nbuild ok\n</monitor>\n\
             <monitor id=\"1\" description=\"ci\" log=\"/logs/1.log\" ended=\"exited with code 1\" events=\"1\"/>"
        );
    }

    #[tokio::test]
    async fn stop_remembers_who_stopped_it() {
        let (monitors, _rx) = monitors();
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
        let (monitors, mut rx) = monitors();
        let m = monitors.register("ci", "watch").await.unwrap();
        line(&monitors, m.id, "before").await;
        monitors.forget_all();
        assert!(m.stop.is_cancelled());
        assert_eq!(monitors.running(), 0);
        let ended = MonitorEvent::Ended {
            id: m.id,
            end: MonitorEnd::Stopped(StoppedBy::Exit),
            events: 1,
        };
        assert!(monitors.event(ended.clone()).await, "the tab still hears");
        assert_eq!(monitors.take_notices(), None);
        while let Ok(event) = rx.try_recv() {
            if matches!(event, MonitorEvent::Ended { .. }) {
                return;
            }
        }
        panic!("the front-end never heard it ended");
    }
}
