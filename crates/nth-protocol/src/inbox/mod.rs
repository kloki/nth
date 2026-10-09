//! The model's inbox: what it has not heard yet, handed over between its
//! steps or as a turn of its own when idle. Monitors put their output here
//! and subagents their answers; `notice` is the text that carries them,
//! written for the model and read back by the transcript. A prompt you send
//! while a turn runs waits here too, and pre-empts its next tool calls.

mod notice;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub use notice::{NOTICE_LINES, NoticeSummary, TaskId, TaskNotice, TaskOutcome, split_notices};

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
    Monitor(MonitorNotice),
    Task(TaskNotice),
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

    /// Whether the model has notices waiting.
    pub fn has_notices(&self) -> bool {
        self.0.as_ref().is_some_and(|p| !lock(p).is_empty())
    }

    /// The prompts waiting, oldest first, and forgets them. Notices stay
    /// for [`Inbox::take_notices`]: a turn that pre-empts its tool calls
    /// takes only these.
    pub fn take_prompts(&self) -> Option<String> {
        let pending = self.0.as_ref()?;
        let mut pending = lock(pending);
        let mut prompts = Vec::new();
        for item in std::mem::take(&mut *pending) {
            match item {
                Pending::Prompt(text) => prompts.push(text),
                notice => pending.push(notice),
            }
        }
        (!prompts.is_empty()).then(|| prompts.join("\n\n"))
    }

    /// The prompts waiting, without forgetting them: the status bar counts
    /// and names them.
    pub fn pending_prompts(&self) -> Vec<String> {
        let Some(pending) = &self.0 else {
            return Vec::new();
        };
        lock(pending)
            .iter()
            .filter_map(|pending| match pending {
                Pending::Prompt(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Everything the model has not seen, as one message, and forgets it:
    /// the background's notices first, so the transcript reads them back,
    /// then any prompts, as a turn's own prompt follows its notices.
    pub fn take_notices(&self) -> Option<String> {
        let pending = std::mem::take(&mut *lock(self.0.as_ref()?));
        if pending.is_empty() {
            return None;
        }
        let mut parts: Vec<String> = pending.iter().filter_map(Pending::notice).collect();
        let prompts: Vec<String> = pending
            .iter()
            .filter_map(|pending| match pending {
                Pending::Prompt(text) => Some(text.clone()),
                _ => None,
            })
            .collect();
        if !prompts.is_empty() {
            parts.push(prompts.join("\n\n"));
        }
        Some(parts.join("\n"))
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
        Pending::Task(_) | Pending::Prompt(_) => {
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
            Pending::Prompt(_) => None,
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
    fn prompts_follow_the_notices_in_take_notices() {
        let inbox = Inbox::new();
        let log = Path::new("/logs/1.log");
        inbox.monitor_line(1, "ci", log, "build ok".into());
        assert!(inbox.post_prompt("hold on".into()));
        assert!(inbox.post_prompt("and thanks".into()));

        assert_eq!(
            inbox.take_notices().unwrap(),
            "<monitor id=\"1\" description=\"ci\" log=\"/logs/1.log\">\nbuild ok\n</monitor>\n\
             hold on\n\nand thanks",
            "notices first, so the transcript's parser keeps working"
        );
        assert_eq!(inbox.take_notices(), None, "taken");
    }

    #[test]
    fn take_prompts_leaves_the_notices_waiting() {
        let inbox = Inbox::new();
        assert!(inbox.post_prompt("one".into()));
        inbox.monitor_line(1, "ci", Path::new("/l/1.log"), "x".into());
        assert!(inbox.post_prompt("two".into()));

        assert_eq!(inbox.pending_prompts(), ["one", "two"]);
        assert_eq!(inbox.take_prompts().as_deref(), Some("one\n\ntwo"));
        assert!(inbox.has_notices(), "the monitor line still waits");
        assert_eq!(inbox.take_prompts(), None, "taken");
        assert_eq!(inbox.pending_prompts(), Vec::<String>::new());
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
}
