//! The model's inbox: what it has not heard yet, handed over between its
//! steps or as a turn of its own when idle. Monitors put their output here
//! and subagents their answers, and work items what happened to them;
//! `notice` is the text that carries them,
//! written for the model and read back by the transcript. A prompt you send
//! while a turn runs waits here too, and pre-empts its next tool calls.

mod notice;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub use notice::{
    NOTICE_LINES, NoticeSummary, TaskId, TaskNotice, TaskOutcome, Todo, WorkNotice, WorkOutcome,
    WorkState, instructions_notice, one_line, quiet_text, split_notices, todo_counts, todo_lines,
};

use crate::{MonitorEnd, MonitorId};

/// The inbox, shared by whoever posts to it and the front-end and agent
/// loop that empty it. The default is none, as in a headless run: nothing
/// would wake the model for a notice, so whoever has one hands it over
/// itself.
#[derive(Debug, Clone, Default)]
pub struct Inbox(Option<Arc<Mutex<Vec<Pending>>>>);

/// One thing waiting for the model.
#[derive(Debug)]
enum Pending {
    Curio(crate::CurioNotice),
    Monitor(MonitorNotice),
    Task(TaskNotice),
    Work(WorkNotice),
    /// A prompt sent while a turn runs, for its next step.
    Prompt(String),
}

#[derive(Debug)]
struct MonitorNotice {
    id: MonitorId,
    description: String,
    log: PathBuf,
    lines: Vec<String>,
    /// Lines past [`NOTICE_LINES`].
    more: usize,
    ended: Option<(MonitorEnd, usize)>,
}

impl Inbox {
    pub fn post_curio(&self, notice: crate::CurioNotice) -> bool {
        let Some(pending) = &self.0 else { return false };
        let mut pending = lock(pending);
        if pending.iter().any(|p| matches!(p, Pending::Curio(n) if n.batch.session_id == notice.batch.session_id && n.batch.id == notice.batch.id)) {
            return false;
        }
        pending.push(Pending::Curio(notice));
        true
    }

    /// Typed delivery bypasses prompt/command/mention parsing and is never preemptive.
    pub fn take_curios(&self, owner: &str) -> Vec<crate::CurioNotice> {
        let Some(pending) = &self.0 else {
            return Vec::new();
        };
        let mut pending = lock(pending);
        let mut taken = Vec::new();
        pending.retain(|p| {
            if let Pending::Curio(n) = p
                && n.batch.session_id == owner
            {
                taken.push(n.clone());
                return false;
            }
            true
        });
        taken
    }
    /// An empty inbox someone will empty.
    pub fn new() -> Self {
        Self(Some(Arc::default()))
    }

    /// Whether a notice posted here reaches the model later, which is what
    /// a tool checks before leaving work to the background.
    pub fn reaches_model(&self) -> bool {
        self.0.is_some()
    }

    /// Puts a subagent's answer in. `false` without an inbox.
    pub fn post_task(&self, task: TaskNotice) -> bool {
        let Some(pending) = &self.0 else { return false };
        lock(pending).push(Pending::Task(task));
        true
    }

    /// Puts what happened to a work item in. A todo list replaces the
    /// item's latest notice when that is a todo list too, so a burst of
    /// ticks reads as one notice with the last list; after a return or a
    /// stall it starts a new one, so the order stays as it happened.
    /// `false` without an inbox.
    pub fn post_work(&self, notice: WorkNotice) -> bool {
        let Some(pending) = &self.0 else { return false };
        let mut pending = lock(pending);
        let latest = pending.iter_mut().rev().find_map(|p| match p {
            Pending::Work(w) if w.cycle == notice.cycle && w.item == notice.item => Some(w),
            _ => None,
        });
        match latest {
            Some(latest)
                if matches!(latest.state, WorkState::Todo(_))
                    && matches!(notice.state, WorkState::Todo(_)) =>
            {
                *latest = notice;
            }
            _ => pending.push(Pending::Work(notice)),
        }
        true
    }

    /// Puts a prompt sent while a turn runs in; the turn's next tool calls
    /// give way to it. `false` without an inbox.
    pub fn post_prompt(&self, text: String) -> bool {
        let Some(pending) = &self.0 else { return false };
        lock(pending).push(Pending::Prompt(text));
        true
    }

    /// A line monitor `id` printed: it joins the monitor's open notice, or
    /// starts one.
    pub fn monitor_line(&self, id: MonitorId, description: &str, log: &Path, line: String) {
        let Some(pending) = &self.0 else { return };
        let mut pending = lock(pending);
        let notice = monitor_notice(&mut pending, id, description, log);
        if notice.lines.len() < NOTICE_LINES {
            notice.lines.push(line);
        } else {
            notice.more += 1;
        }
    }

    /// Monitor `id` ended: its open notice closes with how, and nothing
    /// joins it after.
    pub fn monitor_ended(
        &self,
        id: MonitorId,
        description: &str,
        log: &Path,
        end: MonitorEnd,
        events: usize,
    ) {
        let Some(pending) = &self.0 else { return };
        let mut pending = lock(pending);
        monitor_notice(&mut pending, id, description, log).ended = Some((end, events));
    }

    /// Whether anything waits for the model, notices or prompts.
    pub fn has_notices(&self) -> bool {
        self.0.as_ref().is_some_and(|p| !lock(p).is_empty())
    }

    /// Whether anything waiting is worth waking the model for: anything
    /// but a work item's todo ticks, which go along with the next wake or
    /// between the steps of a turn that runs. Ticks of parallel items would
    /// otherwise each cost a turn whose only answer is "noted".
    pub fn wakes(&self) -> bool {
        self.0.as_ref().is_some_and(|p| {
            lock(p).iter().any(|pending| {
                !matches!(
                    pending,
                    Pending::Work(WorkNotice {
                        state: WorkState::Todo(_),
                        ..
                    })
                )
            })
        })
    }

    /// The prompts waiting, oldest first, and forgets them. Notices stay
    /// for [`Inbox::take_notices`]: a turn that pre-empts its tool calls
    /// takes only these.
    pub fn take_prompts(&self) -> Option<String> {
        let mut pending = lock(self.0.as_ref()?);
        let (prompts, notices): (Vec<_>, Vec<_>) = std::mem::take(&mut *pending)
            .into_iter()
            .partition(|pending| pending.prompt().is_some());
        *pending = notices;
        drop(pending);
        let prompts: Vec<&str> = prompts.iter().filter_map(Pending::prompt).collect();
        (!prompts.is_empty()).then(|| prompts.join("\n\n"))
    }

    /// How many prompts wait and the first line of the oldest, without
    /// forgetting them: the status bar counts and names them on every draw.
    pub fn prompts_waiting(&self) -> Option<(usize, String)> {
        let pending = lock(self.0.as_ref()?);
        let mut prompts = pending.iter().filter_map(Pending::prompt);
        let first = prompts.next()?;
        let line = first
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
            .to_string();
        Some((1 + prompts.count(), line))
    }

    /// The background's notices, as one message, and forgets them. Prompts
    /// stay for [`Inbox::take_prompts`]: they read as what you typed, and
    /// whoever hands them over treats them as a prompt.
    pub fn take_notices(&self) -> Option<String> {
        let mut pending = lock(self.0.as_ref()?);
        let (prompts, notices): (Vec<_>, Vec<_>) = std::mem::take(&mut *pending)
            .into_iter()
            .partition(|pending| {
                pending.prompt().is_some() || matches!(pending, Pending::Curio(_))
            });
        *pending = prompts;
        drop(pending);
        let notices: Vec<String> = notices.iter().filter_map(Pending::notice).collect();
        (!notices.is_empty()).then(|| notices.join("\n"))
    }

    /// Forgets every notice, for a session that was left: they were for
    /// that conversation's model.
    pub fn clear(&self) {
        if let Some(pending) = &self.0 {
            lock(pending).clear();
        }
    }
}

fn lock(pending: &Mutex<Vec<Pending>>) -> std::sync::MutexGuard<'_, Vec<Pending>> {
    pending.lock().expect("inbox lock poisoned")
}

/// The monitor's notice still being filled: the last one, unless it has
/// already ended; else a new one.
fn monitor_notice<'a>(
    pending: &'a mut Vec<Pending>,
    id: MonitorId,
    description: &str,
    log: &Path,
) -> &'a mut MonitorNotice {
    let open = pending
        .iter()
        .rposition(|p| matches!(p, Pending::Monitor(m) if m.id == id && m.ended.is_none()));
    let index = match open {
        Some(index) => index,
        None => {
            pending.push(Pending::Monitor(MonitorNotice {
                id,
                description: description.to_string(),
                log: log.to_path_buf(),
                lines: Vec::new(),
                more: 0,
                ended: None,
            }));
            pending.len() - 1
        }
    };
    match &mut pending[index] {
        Pending::Monitor(monitor) => monitor,
        // Just pushed or found as a monitor's.
        Pending::Task(_) | Pending::Work(_) | Pending::Prompt(_) | Pending::Curio(_) => {
            unreachable!("the index is a monitor's notice")
        }
    }
}

impl Pending {
    /// The notice as the model reads it; a prompt is none, since it reads
    /// as what you typed, not as a notice.
    fn notice(&self) -> Option<String> {
        match self {
            Pending::Monitor(m) => Some(notice::render(
                m.id,
                &m.description,
                &m.log,
                &m.lines,
                m.more,
                m.ended,
            )),
            Pending::Task(task) => Some(notice::render_task(task)),
            Pending::Work(work) => Some(notice::render_work(work)),
            Pending::Prompt(_) => None,
            Pending::Curio(_) => None,
        }
    }

    /// What you typed, for a prompt.
    fn prompt(&self) -> Option<&str> {
        match self {
            Pending::Prompt(text) => Some(text),
            Pending::Monitor(_) | Pending::Task(_) | Pending::Work(_) | Pending::Curio(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> TaskNotice {
        TaskNotice {
            id: 1,
            agent: "explore".into(),
            description: "find tabs".into(),
            outcome: TaskOutcome::Completed("in content.rs".into()),
        }
    }

    #[test]
    fn headless_has_no_inbox() {
        let inbox = Inbox::default();
        assert!(!inbox.reaches_model());
        assert!(!inbox.post_task(task()));
        assert!(!inbox.post_prompt("hold on".into()));
        inbox.monitor_line(1, "ci", Path::new("/l/1.log"), "x".into());
        assert!(!inbox.has_notices());
        assert_eq!(inbox.take_notices(), None);
    }

    #[test]
    fn take_notices_leaves_the_prompts() {
        let inbox = Inbox::new();
        let log = Path::new("/logs/1.log");
        inbox.monitor_line(1, "ci", log, "build ok".into());
        assert!(inbox.post_prompt("hold on".into()));

        assert_eq!(
            inbox.take_notices().unwrap(),
            "<monitor id=\"1\" description=\"ci\" log=\"/logs/1.log\">\nbuild ok\n</monitor>"
        );
        assert_eq!(inbox.take_notices(), None, "taken");
        assert_eq!(inbox.take_prompts().as_deref(), Some("hold on"));
    }

    #[test]
    fn take_prompts_leaves_the_notices_waiting() {
        let inbox = Inbox::new();
        assert!(inbox.post_prompt("\none\nmore".into()));
        inbox.monitor_line(1, "ci", Path::new("/l/1.log"), "x".into());
        assert!(inbox.post_prompt("two".into()));

        assert_eq!(inbox.prompts_waiting(), Some((2, "one".to_string())));
        assert_eq!(inbox.take_prompts().as_deref(), Some("\none\nmore\n\ntwo"));
        assert!(inbox.has_notices(), "the monitor line still waits");
        assert_eq!(inbox.take_prompts(), None, "taken");
        assert_eq!(inbox.prompts_waiting(), None);
    }

    #[test]
    fn a_monitors_lines_join_its_open_notice_and_an_answer_keeps_its_place() {
        let inbox = Inbox::new();
        let log = Path::new("/logs/1.log");
        inbox.monitor_line(1, "ci", log, "build ok".into());
        assert!(inbox.post_task(task()));
        inbox.monitor_line(1, "ci", log, "tests ok".into());
        inbox.monitor_ended(1, "ci", log, MonitorEnd::Exited(Some(0)), 2);
        inbox.monitor_line(1, "ci", log, "late".into());

        assert!(inbox.has_notices());
        assert_eq!(
            inbox.take_notices().unwrap(),
            "<monitor id=\"1\" description=\"ci\" log=\"/logs/1.log\">\nbuild ok\ntests ok\n</monitor>\n\
             <monitor id=\"1\" description=\"ci\" log=\"/logs/1.log\" ended=\"exited with code 0\" events=\"2\"/>\n\
             <task id=\"1\" agent=\"explore\" description=\"find tabs\" state=\"completed\">\n\
             <task_result>\nin content.rs\n</task_result>\n</task>\n\
             <monitor id=\"1\" description=\"ci\" log=\"/logs/1.log\">\nlate\n</monitor>",
            "an ended notice is closed; a later line starts a new one"
        );
        assert_eq!(inbox.take_notices(), None, "taken");
    }

    #[test]
    fn a_notice_caps_its_lines_and_clear_forgets_them() {
        let inbox = Inbox::new();
        let log = Path::new("/logs/1.log");
        for i in 0..NOTICE_LINES + 3 {
            inbox.monitor_line(1, "spam", log, i.to_string());
        }
        let notices = inbox.clone();
        assert!(
            notices
                .take_notices()
                .unwrap()
                .contains("\n49\n… 3 more lines in the log\n</monitor>")
        );

        inbox.post_task(task());
        inbox.clear();
        assert!(!inbox.has_notices(), "shared, and cleared");
    }
    fn work(item: &str, state: WorkState) -> WorkNotice {
        WorkNotice {
            cycle: "01-a".into(),
            item: item.into(),
            task: 1,
            agent: "general".into(),
            state,
        }
    }

    fn ticked(done: usize) -> WorkState {
        WorkState::Todo(
            (0..2)
                .map(|i| Todo {
                    text: format!("step {i}"),
                    done: i < done,
                })
                .collect(),
        )
    }

    #[test]
    fn a_burst_of_ticks_is_one_notice_and_a_return_keeps_its_place() {
        let inbox = Inbox::new();
        let returned = WorkState::Returned {
            outcome: WorkOutcome::Completed("ok".into()),
            done: 2,
            total: 2,
        };
        assert!(inbox.post_work(work("x", ticked(0))));
        inbox.post_work(work("y", ticked(1)));
        inbox.post_work(work("x", ticked(1)));
        inbox.post_work(work("x", ticked(2)));
        inbox.post_work(work("x", returned.clone()));
        inbox.post_work(work("x", ticked(0)));

        let text = inbox.take_notices().unwrap();
        let (notices, _) = split_notices(&text);
        type Seen = (String, String, Option<(usize, usize)>);
        let seen: Vec<Seen> = notices
            .into_iter()
            .map(|notice| match notice {
                NoticeSummary::Work {
                    item,
                    state,
                    progress,
                    ..
                } => (item, state, progress),
                other => panic!("{other:?}"),
            })
            .collect();
        let row =
            |item: &str, state: &str, done| (item.to_string(), state.to_string(), Some((done, 2)));
        assert_eq!(
            seen,
            [
                row("x", "todo", 2),
                row("y", "todo", 1),
                row("x", "returned", 2),
                row("x", "todo", 0),
            ],
            "the first list took the later ticks; the one after the return stands alone"
        );
        assert!(!Inbox::default().post_work(work("x", returned.clone())));

        inbox.post_work(work("x", ticked(1)));
        assert!(inbox.has_notices());
        assert!(!inbox.wakes(), "ticks wait for a wake");
        inbox.post_work(work("y", returned));
        assert!(inbox.wakes(), "a return is one");
    }
}
