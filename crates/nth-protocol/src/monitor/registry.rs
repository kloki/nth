//! The front-end's monitors: which are running and who stopped them. What
//! they print goes to the front-end, which puts it in the model's inbox.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use tokio::sync::{
    mpsc::{self, error::TrySendError},
    oneshot,
};
use tokio_util::sync::CancellationToken;

use super::{MonitorEvent, MonitorId, Registered, StdinWrite, StoppedBy};

/// The front-end's monitors, shared by the monitor tools that start and
/// stop them and the front-end that shows them. The default has no
/// front-end, as in a headless run, so nobody would hear from a monitor
/// and none can start.
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
}

#[derive(Debug)]
struct Running {
    stop: CancellationToken,
    stopped_by: Option<StoppedBy>,
    stdin: Stdin,
}

/// A monitor's stdin, as far as the model can still write to it.
#[derive(Debug)]
enum Stdin {
    /// Started without one: the command reads end-of-input at once.
    None,
    /// Writes go to the task that owns the pipe.
    Open(mpsc::Sender<StdinWrite>),
    /// The model closed it, or the task writing it has gone.
    Closed,
}

/// Why [`Monitors::input`] could not hand text to a monitor's stdin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputRefused {
    NoMonitor,
    /// It was started without `stdin`.
    NoStdin,
    Closed,
    /// Writes the command has not read yet fill the queue in front of its
    /// pipe.
    Backlogged,
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
    /// there is no front-end to hear from it. `stdin` is where the model's
    /// writes to it go, for a monitor started with an open stdin.
    pub async fn register(
        &self,
        description: &str,
        command: &str,
        stdin: Option<mpsc::Sender<StdinWrite>>,
    ) -> Option<Registered> {
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
                    stop: stop.clone(),
                    stopped_by: None,
                    stdin: stdin.map_or(Stdin::None, Stdin::Open),
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

    /// Hands `text` to the task that writes a running monitor's stdin, and
    /// closes it after when `close` is set. The receiver hears once it is
    /// written, or why it could not be; it errs when the task went first.
    /// Never waits: a command that stopped reading holds up its own writes,
    /// never the caller's lock or the monitors of others.
    pub fn input(
        &self,
        id: MonitorId,
        text: String,
        close: bool,
    ) -> Result<oneshot::Receiver<Result<(), String>>, InputRefused> {
        let inner = self.0.as_ref().ok_or(InputRefused::NoMonitor)?;
        let mut state = inner.lock();
        let running = state.running.get_mut(&id).ok_or(InputRefused::NoMonitor)?;
        let writer = match &running.stdin {
            Stdin::None => return Err(InputRefused::NoStdin),
            Stdin::Closed => return Err(InputRefused::Closed),
            Stdin::Open(writer) => writer,
        };
        let (done, written) = oneshot::channel();
        match writer.try_send(StdinWrite { text, close, done }) {
            Ok(()) => {
                // Closed as it is queued, so a write after it is refused
                // here rather than lost behind the close.
                if close {
                    running.stdin = Stdin::Closed;
                }
                Ok(written)
            }
            Err(TrySendError::Full(_)) => Err(InputRefused::Backlogged),
            // The task ends after a close or a failed write.
            Err(TrySendError::Closed(_)) => {
                running.stdin = Stdin::Closed;
                Err(InputRefused::Closed)
            }
        }
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

    /// Stops every running monitor for a session that was left, and
    /// returns the highest number handed out so far: every monitor up to it
    /// ran for that session, so the front-end keeps what they still say
    /// from the next session's model. Read under the same lock that numbers
    /// new monitors, so one registered meanwhile is on one side or the other.
    pub fn forget_all(&self) -> MonitorId {
        let Some(inner) = &self.0 else { return 0 };
        let mut state = inner.lock();
        for (_, running) in state.running.drain() {
            running.stop.cancel();
        }
        state.next
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

    /// Passes a monitor's output or end on to the front-end; an end also
    /// stops counting it as running. `false` once the front-end is gone,
    /// when the monitor should stop: nobody is left to hear from it.
    pub async fn event(&self, event: MonitorEvent) -> bool {
        let Some(inner) = &self.0 else { return false };
        if let MonitorEvent::Ended { id, .. } = &event {
            inner.lock().running.remove(id);
        }
        inner.front_end.send(event).await.is_ok()
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("monitor state lock poisoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MonitorEnd;

    fn monitors() -> (Monitors, mpsc::Receiver<MonitorEvent>) {
        let (tx, rx) = mpsc::channel(64);
        (Monitors::new(tx, "/logs".into()), rx)
    }

    #[tokio::test]
    async fn headless_has_no_monitors() {
        let monitors = Monitors::default();
        assert!(monitors.register("tail", "tail -f x", None).await.is_none());
        assert_eq!(
            monitors.input(1, "x".into(), false).unwrap_err(),
            InputRefused::NoMonitor
        );
        assert!(!monitors.reaches_front_end());
    }

    #[tokio::test]
    async fn register_numbers_and_announces() {
        let (monitors, mut rx) = monitors();
        let first = monitors
            .register("errors", "tail -f log", None)
            .await
            .unwrap();
        let second = monitors.register("ci", "gh run watch", None).await.unwrap();
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
    async fn an_end_reaches_the_front_end_and_stops_running() {
        let (monitors, mut rx) = monitors();
        let m = monitors.register("ci", "watch", None).await.unwrap();
        let ended = MonitorEvent::Ended {
            id: m.id,
            end: MonitorEnd::Exited(Some(1)),
            events: 0,
        };
        assert!(monitors.event(ended.clone()).await);

        assert_eq!(monitors.running(), 0);
        assert!(matches!(
            rx.recv().await,
            Some(MonitorEvent::Started { .. })
        ));
        assert_eq!(rx.recv().await, Some(ended));
    }

    #[tokio::test]
    async fn stop_remembers_who_stopped_it() {
        let (monitors, _rx) = monitors();
        let m = monitors.register("ci", "watch", None).await.unwrap();
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
    async fn forgetting_stops_them_and_says_up_to_which_number() {
        let (monitors, mut rx) = monitors();
        assert_eq!(
            Monitors::default().forget_all(),
            0,
            "none without a front-end"
        );
        let m = monitors.register("ci", "watch", None).await.unwrap();
        monitors.register("logs", "tail", None).await.unwrap();

        assert_eq!(monitors.forget_all(), 2);
        assert!(m.stop.is_cancelled());
        assert_eq!(monitors.running(), 0);
        let next = monitors.register("new", "watch", None).await.unwrap();
        assert_eq!(next.id, 3, "numbers go on across sessions");

        let ended = MonitorEvent::Ended {
            id: m.id,
            end: MonitorEnd::Stopped(StoppedBy::Exit),
            events: 1,
        };
        assert!(monitors.event(ended.clone()).await, "the tab still hears");
        while let Ok(event) = rx.try_recv() {
            if event == ended {
                return;
            }
        }
        panic!("the front-end never heard it ended");
    }

    #[tokio::test]
    async fn input_reaches_the_writer_and_close_shuts_it() {
        let (monitors, _rx) = monitors();
        let (tx, mut writes) = mpsc::channel(1);
        let m = monitors
            .register("repl", "python3", Some(tx))
            .await
            .unwrap();

        let written = monitors.input(m.id, "1+1\n".into(), false).unwrap();
        let write = writes.recv().await.expect("queued");
        assert_eq!((write.text.as_str(), write.close), ("1+1\n", false));
        write.done.send(Ok(())).unwrap();
        assert_eq!(written.await, Ok(Ok(())));

        monitors.input(m.id, String::new(), true).unwrap();
        assert!(writes.recv().await.expect("queued").close);
        assert_eq!(
            monitors.input(m.id, "late\n".into(), false).unwrap_err(),
            InputRefused::Closed,
            "nothing goes in after a close"
        );
    }

    #[tokio::test]
    async fn input_is_refused_without_a_stdin_to_write() {
        let (monitors, _rx) = monitors();
        let plain = monitors.register("ci", "watch", None).await.unwrap();
        assert_eq!(
            monitors.input(plain.id, "x".into(), false).unwrap_err(),
            InputRefused::NoStdin
        );
        assert_eq!(
            monitors.input(9, "x".into(), false).unwrap_err(),
            InputRefused::NoMonitor
        );

        // Room for one write the command has not read.
        let (tx, writes) = mpsc::channel(1);
        let m = monitors.register("repl", "cat", Some(tx)).await.unwrap();
        monitors.input(m.id, "a".into(), false).unwrap();
        assert_eq!(
            monitors.input(m.id, "b".into(), false).unwrap_err(),
            InputRefused::Backlogged
        );

        // The writer gone, as after a failed write.
        drop(writes);
        assert_eq!(
            monitors.input(m.id, "c".into(), false).unwrap_err(),
            InputRefused::Closed
        );
        assert_eq!(
            monitors.input(m.id, "d".into(), false).unwrap_err(),
            InputRefused::Closed
        );

        monitors.forget_all();
        assert_eq!(
            monitors.input(m.id, "e".into(), false).unwrap_err(),
            InputRefused::NoMonitor
        );
    }
}
