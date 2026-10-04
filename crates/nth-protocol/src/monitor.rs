//! Background monitors: commands the model leaves running whose output
//! comes back to it as notices, between steps or as a turn of their own.

use std::{
    collections::HashMap,
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Numbered from 1 for as long as the front-end runs, so a monitor keeps its
/// number across sessions.
pub type MonitorId = u32;

/// At most this many lines of one monitor go into a notice; the rest are
/// counted, and the log has them.
pub const NOTICE_LINES: usize = 50;

/// What a monitor reports to the front-end, for its tab.
#[derive(Debug, Clone, PartialEq)]
pub enum MonitorEvent {
    Started {
        id: MonitorId,
        description: String,
        command: String,
        log: PathBuf,
    },
    /// One line of output. Only stdout lines are events for the model.
    Output {
        id: MonitorId,
        line: String,
        stream: Stream,
    },
    Ended {
        id: MonitorId,
        end: MonitorEnd,
        /// How many stdout lines it produced.
        events: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorEnd {
    /// The command exited by itself; `None` when a signal killed it.
    Exited(Option<i32>),
    TimedOut {
        after_ms: u64,
    },
    /// It printed more than the model could take in.
    Flooded,
    Stopped(StoppedBy),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoppedBy {
    Model,
    User,
    /// nth quit, or the session it ran for was left.
    Exit,
}

impl MonitorEnd {
    /// Whether it ended the way a finished command should.
    pub fn is_success(self) -> bool {
        self == MonitorEnd::Exited(Some(0))
    }
}

impl fmt::Display for MonitorEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MonitorEnd::Exited(Some(code)) => write!(f, "exited with code {code}"),
            MonitorEnd::Exited(None) => write!(f, "killed by a signal"),
            MonitorEnd::TimedOut { after_ms } => {
                write!(
                    f,
                    "timed out after {}s, start it again to keep watching",
                    after_ms / 1000
                )
            }
            MonitorEnd::Flooded => {
                write!(
                    f,
                    "stopped: too many events, start it again with a tighter filter"
                )
            }
            MonitorEnd::Stopped(StoppedBy::Model) => write!(f, "stopped with monitor_stop"),
            MonitorEnd::Stopped(StoppedBy::User) => write!(f, "stopped by the user"),
            MonitorEnd::Stopped(StoppedBy::Exit) => write!(f, "stopped: nth exited"),
        }
    }
}

/// What the monitor tool gets for a new monitor.
#[derive(Debug)]
pub struct Registered {
    pub id: MonitorId,
    /// Cancelled when someone stops it; the monitor then asks
    /// [`Monitors::stopped_by`] who.
    pub stop: CancellationToken,
    /// Where it writes every line it reads, stdout and stderr.
    pub log: PathBuf,
}

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
    /// Lines past [`NOTICE_LINES`].
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
                if pending.lines.len() < NOTICE_LINES {
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
    /// The lines in a `<monitor>` element, then how it ended in an empty
    /// one, as the transcript reads them back.
    fn notice(&self) -> String {
        let attributes = format!(
            "id=\"{}\" description=\"{}\" log=\"{}\"",
            self.id,
            attribute(&self.description),
            attribute(&self.log.display().to_string()),
        );
        let mut out = Vec::new();
        if !self.lines.is_empty() {
            out.push(format!("<monitor {attributes}>"));
            out.extend(self.lines.iter().cloned());
            if self.more > 0 {
                out.push(format!("… {} more lines in the log", self.more));
            }
            out.push("</monitor>".into());
        }
        if let Some((end, events)) = self.ended {
            out.push(format!(
                "<monitor {attributes} ended=\"{}\" events=\"{events}\"/>",
                attribute(&end.to_string())
            ));
        }
        out.join("\n")
    }
}

fn attribute(text: &str) -> String {
    text.replace('&', "&amp;").replace('"', "&quot;")
}

/// A monitor's notice as the transcript shows it, from the text the model
/// read: which monitor, and how many lines or how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeSummary {
    pub id: MonitorId,
    pub description: String,
    pub lines: usize,
    pub ended: Option<String>,
}

/// Splits the notices off the front of a user message: what a monitor
/// said, then what the user typed after it, if anything.
pub fn split_notices(text: &str) -> (Vec<NoticeSummary>, &str) {
    let mut notices = Vec::new();
    let mut rest = text;
    while let Some(after) = rest.strip_prefix("<monitor ") {
        let Some(tag_end) = after.find('>') else {
            break;
        };
        let tag = &after[..tag_end];
        let (attrs, closed) = match tag.strip_suffix('/') {
            Some(attrs) => (attrs, true),
            None => (tag, false),
        };
        let Some(id) = value(attrs, "id").and_then(|id| id.parse().ok()) else {
            break;
        };
        let description = value(attrs, "description").unwrap_or_default();
        let body = &after[tag_end + 1..];
        let (lines, next) = if closed {
            (0, body)
        } else {
            let Some(close) = body.find("\n</monitor>") else {
                break;
            };
            let lines = body[..close].lines().filter(|l| !l.is_empty()).count();
            (lines, &body[close + "\n</monitor>".len()..])
        };
        notices.push(NoticeSummary {
            id,
            description,
            lines,
            ended: value(attrs, "ended"),
        });
        rest = next.strip_prefix('\n').unwrap_or(next);
    }
    if notices.is_empty() {
        return (notices, text);
    }
    (notices, rest.trim_start_matches('\n'))
}

fn value(attrs: &str, name: &str) -> Option<String> {
    let start = attrs.find(&format!("{name}=\""))? + name.len() + 2;
    let len = attrs[start..].find('"')?;
    Some(
        attrs[start..start + len]
            .replace("&quot;", "\"")
            .replace("&amp;", "&"),
    )
}

/// Where a monitor's log goes, under nth's data folder, per session.
pub fn log_dir(data_dir: &Path, session_id: &str) -> PathBuf {
    data_dir.join("monitors").join(session_id)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn notices_split_off_the_front_of_a_message() {
        let (monitors, _rx) = monitors();
        let m = monitors.register("ci \"main\"", "watch").await.unwrap();
        line(&monitors, m.id, "step 1 ok").await;
        line(&monitors, m.id, "step 2 ok").await;
        let ended = MonitorEvent::Ended {
            id: m.id,
            end: MonitorEnd::Exited(Some(0)),
            events: 2,
        };
        monitors.event(ended).await;
        let text = format!("{}\n\nnow fix it", monitors.take_notices().unwrap());

        let (notices, rest) = split_notices(&text);
        assert_eq!(
            notices,
            [
                NoticeSummary {
                    id: 1,
                    description: "ci \"main\"".into(),
                    lines: 2,
                    ended: None,
                },
                NoticeSummary {
                    id: 1,
                    description: "ci \"main\"".into(),
                    lines: 0,
                    ended: Some("exited with code 0".into()),
                },
            ]
        );
        assert_eq!(rest, "now fix it");
        assert_eq!(split_notices("hi"), (vec![], "hi"));
    }
}
