//! The notices the model reads between its steps: a `<monitor>` element for
//! what a monitor said, a `<task>` element for what a subagent answered, a
//! `<work_item>` element for what happened to a work item, an
//! `<instructions>` element for an instruction file that changed, and what
//! the transcript reads back out of them. Both sides are here so they cannot
//! drift apart.

use std::{fmt, path::Path, time::Duration};

use nth_context::instructions::{Change, ChangeKind};

use crate::{MonitorEnd, MonitorId};

/// At most this many lines of one monitor go into a notice; the rest are
/// counted, and the log has them.
pub const NOTICE_LINES: usize = 50;

/// Numbers a subagent for as long as the front-end runs, like a monitor.
pub type TaskId = u32;

/// A subagent's answer to a task, posted by whoever ran it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskNotice {
    pub id: TaskId,
    pub agent: String,
    pub description: String,
    pub outcome: TaskOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TaskOutcome {
    /// The subagent's last answer.
    Completed(String),
    /// Why its turn failed.
    Failed(String),
    /// Someone stopped it before it answered.
    Interrupted,
}

/// One step of a work item's todo list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    pub text: String,
    pub done: bool,
}

/// How the subagent on a work item ended its turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkOutcome {
    /// Its last answer.
    Completed(String),
    /// Why its turn failed.
    Failed(String),
    /// Someone stopped it before it answered.
    Interrupted,
    /// It ran past its time; the error says how long that was.
    TimedOut(String),
}

impl WorkOutcome {
    /// The `outcome` attribute, and the item file's `outcome` key.
    pub fn name(&self) -> &'static str {
        match self {
            WorkOutcome::Completed(_) => "completed",
            WorkOutcome::Failed(_) => "failed",
            WorkOutcome::Interrupted => "interrupted",
            WorkOutcome::TimedOut(_) => "timed_out",
        }
    }

    /// The outcome a name stands for, without its text: an item file keeps
    /// the text apart, under its result.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "completed" => WorkOutcome::Completed(String::new()),
            "failed" => WorkOutcome::Failed(String::new()),
            "interrupted" => WorkOutcome::Interrupted,
            "timed_out" => WorkOutcome::TimedOut(String::new()),
            _ => return None,
        })
    }

    /// The answer or the error, empty for a stopped one.
    pub fn text(&self) -> &str {
        match self {
            WorkOutcome::Completed(text)
            | WorkOutcome::Failed(text)
            | WorkOutcome::TimedOut(text) => text,
            WorkOutcome::Interrupted => "",
        }
    }
}

impl fmt::Display for WorkOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What happened to a work item, for the orchestrator that assigned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkNotice {
    /// The cycle's folder, such as `01-foundation`.
    pub cycle: String,
    /// The item's slug in it.
    pub item: String,
    /// The subagent working on it.
    pub task: TaskId,
    pub agent: String,
    pub state: WorkState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkState {
    /// The subagent changed the todo list; the whole list as it is now.
    Todo(Vec<Todo>),
    /// The subagent's turn ended, with how many steps it had ticked.
    Returned {
        outcome: WorkOutcome,
        done: usize,
        total: usize,
    },
    /// Nothing heard from the subagent for this long; it still runs.
    Stalled { quiet: Duration },
}

impl WorkState {
    /// The `state` attribute.
    pub fn name(&self) -> &'static str {
        match self {
            WorkState::Todo(_) => "todo",
            WorkState::Returned { .. } => "returned",
            WorkState::Stalled { .. } => "stalled",
        }
    }
}

impl WorkNotice {
    /// The notice as the model reads it, for a caller that hands it over
    /// itself rather than through the inbox.
    pub fn render(&self) -> String {
        render_work(self)
    }
}

/// The todo list as markdown checkboxes, one step a line, the way an item
/// file keeps it. A step's text stays on its line.
pub fn todo_lines(todos: &[Todo]) -> String {
    todos
        .iter()
        .map(|todo| {
            let mark = if todo.done { 'x' } else { ' ' };
            format!("- [{mark}] {}", one_line(&todo.text))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Ticked steps and all of them.
pub fn todo_counts(todos: &[Todo]) -> (usize, usize) {
    (todos.iter().filter(|todo| todo.done).count(), todos.len())
}

/// Text folded onto one line, for a todo step or a note.
pub fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `2m0s`, or `45s` under a minute: whole seconds.
pub fn quiet_text(quiet: Duration) -> String {
    let secs = quiet.as_secs();
    match secs / 60 {
        0 => format!("{secs}s"),
        minutes => format!("{minutes}m{}s", secs % 60),
    }
}

impl TaskNotice {
    /// The notice as the model reads it, for a caller that hands the answer
    /// over itself rather than through the inbox.
    pub fn render(&self) -> String {
        render_task(self)
    }
}

impl TaskOutcome {
    /// The `state` attribute, as opencode names it.
    pub fn state(&self) -> &'static str {
        match self {
            TaskOutcome::Completed(_) => "completed",
            TaskOutcome::Failed(_) => "failed",
            TaskOutcome::Interrupted => "interrupted",
        }
    }
}

impl fmt::Display for TaskOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.state())
    }
}

/// The lines in a `<monitor>` element, then how it ended in an empty one.
pub(super) fn render(
    id: MonitorId,
    description: &str,
    log: &Path,
    lines: &[String],
    more: usize,
    ended: Option<(MonitorEnd, usize)>,
) -> String {
    let attributes = format!(
        "id=\"{id}\" description=\"{}\" log=\"{}\"",
        attribute(description),
        attribute(&log.display().to_string()),
    );
    let mut out = Vec::new();
    if !lines.is_empty() {
        out.push(format!("<monitor {attributes}>"));
        out.extend(lines.iter().map(|line| body(line, "monitor")));
        if more > 0 {
            out.push(format!("… {more} more lines in the log"));
        }
        out.push("</monitor>".into());
    }
    if let Some((end, events)) = ended {
        out.push(format!(
            "<monitor {attributes} ended=\"{}\" events=\"{events}\"/>",
            attribute(&end.to_string())
        ));
    }
    out.join("\n")
}

/// A `<task>` element in opencode's shape: the answer in a `<task_result>`,
/// an error in a `<task_error>`, and an empty element for one that was
/// stopped.
pub(super) fn render_task(task: &TaskNotice) -> String {
    let attributes = format!(
        "id=\"{}\" agent=\"{}\" description=\"{}\" state=\"{}\"",
        task.id,
        attribute(&task.agent),
        attribute(&task.description),
        task.outcome.state(),
    );
    let body = match &task.outcome {
        TaskOutcome::Completed(text) => ("task_result", text),
        TaskOutcome::Failed(error) => ("task_error", error),
        TaskOutcome::Interrupted => return format!("<task {attributes}/>"),
    };
    format!(
        "<task {attributes}>\n<{tag}>\n{}\n</{tag}>\n</task>",
        self::body(body.1.trim(), "task"),
        tag = body.0
    )
}

/// A `<work_item>` element: the todo list as it is now, the answer in a
/// `<work_result>` or the error in a `<work_error>` once its subagent
/// returned, and an empty element for one that was stopped or went quiet.
pub(super) fn render_work(notice: &WorkNotice) -> String {
    let mut attributes = format!(
        "cycle=\"{}\" item=\"{}\" task=\"{}\" agent=\"{}\" state=\"{}\"",
        attribute(&notice.cycle),
        attribute(&notice.item),
        notice.task,
        attribute(&notice.agent),
        notice.state.name(),
    );
    match &notice.state {
        WorkState::Todo(todos) => {
            let (done, total) = todo_counts(todos);
            attributes.push_str(&format!(" done=\"{done}\" total=\"{total}\""));
            if todos.is_empty() {
                return format!("<work_item {attributes}/>");
            }
            format!(
                "<work_item {attributes}>\n{}\n</work_item>",
                body(&todo_lines(todos), "work_item")
            )
        }
        WorkState::Returned {
            outcome,
            done,
            total,
        } => {
            attributes.push_str(&format!(
                " outcome=\"{}\" done=\"{done}\" total=\"{total}\"",
                outcome.name()
            ));
            let tag = match outcome {
                WorkOutcome::Completed(_) => "work_result",
                WorkOutcome::Failed(_) | WorkOutcome::TimedOut(_) => "work_error",
                WorkOutcome::Interrupted => return format!("<work_item {attributes}/>"),
            };
            let text = body(&body(outcome.text().trim(), tag), "work_item");
            format!("<work_item {attributes}>\n<{tag}>\n{text}\n</{tag}>\n</work_item>")
        }
        WorkState::Stalled { quiet } => {
            attributes.push_str(&format!(" quiet=\"{}\"", quiet_text(*quiet)));
            format!("<work_item {attributes}/>")
        }
    }
}

/// An `<instructions>` element for each file in `changes`, as one notice;
/// `None` when nothing changed. An edit comes as a unified diff when that
/// is shorter than the file, else as the whole file, as opencode sends it.
pub fn instructions_notice(changes: &[Change]) -> Option<String> {
    let notices: Vec<String> = changes.iter().map(render_instructions).collect();
    (!notices.is_empty()).then(|| notices.join("\n"))
}

fn render_instructions(change: &Change) -> String {
    let attributes = format!(
        "path=\"{}\" change=\"{}\"",
        attribute(&change.path.display().to_string()),
        change.kind.name(),
    );
    let (content, text) = match &change.kind {
        ChangeKind::Removed => return format!("<instructions {attributes}/>"),
        ChangeKind::Created(text) => ("whole", text.clone()),
        ChangeKind::Edited { before, after } => {
            let diff = similar::TextDiff::from_lines(before, after)
                .unified_diff()
                .context_radius(3)
                .to_string();
            match diff.len() < after.len() {
                true => ("diff", diff),
                false => ("whole", after.clone()),
            }
        }
    };
    format!(
        "<instructions {attributes} content=\"{content}\">\n{}\n</instructions>",
        body(text.trim_end(), "instructions")
    )
}

/// Keeps a value from ending its attribute or its tag: `split_notices`
/// finds the tag's end at the first `>`, and a newline would read as a
/// line of output.
fn attribute(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\n', "&#10;")
}

/// Keeps text inside an element from closing it: an answer that quotes
/// `</task>` or `</task_result>` would otherwise end the notice early for
/// `split_notices` and for the model.
fn body(text: &str, element: &str) -> String {
    text.replace(&format!("</{element}"), &format!("<\\/{element}"))
}

/// A notice as the transcript shows it, from the text the model read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoticeSummary {
    Curio {
        delivery: String,
        session: String,
        curio: String,
        revision: u64,
    },
    /// Which monitor, and how many lines or how it ended.
    Monitor {
        id: MonitorId,
        description: String,
        lines: usize,
        ended: Option<String>,
    },
    /// Which subagent, and how its task ended.
    Task {
        id: TaskId,
        agent: String,
        description: String,
        state: String,
    },
    /// Which work item, what happened to it and how far its todo is.
    Work {
        cycle: String,
        item: String,
        task: TaskId,
        agent: String,
        state: String,
        outcome: Option<String>,
        /// Ticked steps and all of them; `None` for a stalled item.
        progress: Option<(usize, usize)>,
    },
    /// Which instruction file, and whether it was created, edited or
    /// removed.
    Instructions { path: String, change: String },
}

/// The elements `split_notices` knows.
const ELEMENTS: [&str; 5] = ["monitor", "task", "work_item", "instructions", "curio"];

/// Splits the notices off the front of a user message: what monitors,
/// subagents and work items said, then what the user typed after it, if
/// anything.
pub fn split_notices(text: &str) -> (Vec<NoticeSummary>, &str) {
    let mut notices = Vec::new();
    let mut rest = text;
    while let Some((element, after)) = ELEMENTS
        .into_iter()
        .find_map(|element| Some((element, rest.strip_prefix(&format!("<{element} "))?)))
    {
        let Some(tag_end) = after.find('>') else {
            break;
        };
        let tag = &after[..tag_end];
        let (attrs, closed) = match tag.strip_suffix('/') {
            Some(attrs) => (attrs, true),
            None => (tag, false),
        };
        let body = &after[tag_end + 1..];
        let (inner, next) = if closed {
            ("", body)
        } else {
            let close = format!("\n</{element}>");
            let Some(at) = body.find(&close) else {
                break;
            };
            (&body[..at], &body[at + close.len()..])
        };
        let Some(notice) = summary(element, attrs, inner) else {
            break;
        };
        notices.push(notice);
        rest = next.strip_prefix('\n').unwrap_or(next);
    }
    if notices.is_empty() {
        return (notices, text);
    }
    (notices, rest.trim_start_matches('\n'))
}

/// One element's summary; `None` when an attribute it cannot do without is
/// missing, which ends the notices there.
fn summary(element: &str, attrs: &str, inner: &str) -> Option<NoticeSummary> {
    Some(match element {
        "curio" => NoticeSummary::Curio {
            delivery: value(attrs, "delivery")?,
            session: value(attrs, "session")?,
            curio: value(attrs, "curio")?,
            revision: number(attrs, "revision")?,
        },
        "monitor" => NoticeSummary::Monitor {
            id: number(attrs, "id")?,
            description: value(attrs, "description").unwrap_or_default(),
            lines: inner.lines().filter(|l| !l.is_empty()).count(),
            ended: value(attrs, "ended"),
        },
        "task" => NoticeSummary::Task {
            id: number(attrs, "id")?,
            agent: value(attrs, "agent").unwrap_or_default(),
            description: value(attrs, "description").unwrap_or_default(),
            state: value(attrs, "state").unwrap_or_default(),
        },
        "instructions" => NoticeSummary::Instructions {
            path: value(attrs, "path")?,
            change: value(attrs, "change").unwrap_or_default(),
        },
        _ => NoticeSummary::Work {
            cycle: value(attrs, "cycle")?,
            item: value(attrs, "item")?,
            task: number(attrs, "task")?,
            agent: value(attrs, "agent").unwrap_or_default(),
            state: value(attrs, "state").unwrap_or_default(),
            outcome: value(attrs, "outcome"),
            progress: number(attrs, "done").zip(number(attrs, "total")),
        },
    })
}

fn number<T: std::str::FromStr>(attrs: &str, name: &str) -> Option<T> {
    value(attrs, name)?.parse().ok()
}

fn value(attrs: &str, name: &str) -> Option<String> {
    let start = attrs.find(&format!("{name}=\""))? + name.len() + 2;
    let len = attrs[start..].find('"')?;
    // `&amp;` last, so an escaped ampersand is not unescaped twice.
    Some(
        attrs[start..start + len]
            .replace("&#10;", "\n")
            .replace("&gt;", ">")
            .replace("&lt;", "<")
            .replace("&quot;", "\"")
            .replace("&amp;", "&"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(
        id: MonitorId,
        description: &str,
        lines: usize,
        ended: Option<&str>,
    ) -> NoticeSummary {
        NoticeSummary::Monitor {
            id,
            description: description.into(),
            lines,
            ended: ended.map(Into::into),
        }
    }

    #[test]
    fn notices_split_off_the_front_of_a_message() {
        let log = Path::new("/logs/1.log");
        let lines = ["step 1 ok".to_string(), "step 2 ok".to_string()];
        let output = render(1, "ci \"main\"", log, &lines, 0, None);
        let ended = render(
            1,
            "ci \"main\"",
            log,
            &[],
            0,
            Some((MonitorEnd::Exited(Some(0)), 2)),
        );
        let text = format!("{output}\n{ended}\n\nnow fix it");

        let (notices, rest) = split_notices(&text);
        assert_eq!(
            notices,
            [
                monitor(1, "ci \"main\"", 2, None),
                monitor(1, "ci \"main\"", 0, Some("exited with code 0")),
            ]
        );
        assert_eq!(rest, "now fix it");
        assert_eq!(split_notices("hi"), (vec![], "hi"));
    }

    #[test]
    fn a_description_cannot_end_the_tag_or_start_a_line() {
        let log = Path::new("/logs/2.log");
        let lines = ["GET /health".to_string()];
        for description in ["errors (status > 400)", "first\nsecond", "<b>&amp;</b>"] {
            let output = render(2, description, log, &lines, 0, None);
            let ended = render(2, description, log, &[], 0, Some((MonitorEnd::Flooded, 1)));
            let text = format!("{output}\n{ended}\n\nlook");

            assert_eq!(
                output.lines().count(),
                3,
                "the tag stays on one line: {output}"
            );
            let (notices, rest) = split_notices(&text);
            assert_eq!(
                notices,
                [
                    monitor(2, description, 1, None),
                    monitor(2, description, 0, Some(&MonitorEnd::Flooded.to_string())),
                ],
                "{description:?}"
            );
            assert_eq!(rest, "look");
        }
    }

    #[test]
    fn a_task_notice_carries_the_answer_and_splits_beside_a_monitors() {
        let task = TaskNotice {
            id: 3,
            agent: "explore".into(),
            description: "find \"tabs\"".into(),
            outcome: TaskOutcome::Completed("Tabs open in content.rs.\n\nSee `open`.\n".into()),
        };
        let rendered = render_task(&task);
        assert_eq!(
            rendered,
            "<task id=\"3\" agent=\"explore\" description=\"find &quot;tabs&quot;\" state=\"completed\">\n\
             <task_result>\nTabs open in content.rs.\n\nSee `open`.\n</task_result>\n</task>"
        );
        let monitor_notice = render(1, "ci", Path::new("/l/1.log"), &["ok".into()], 0, None);
        let failed = render_task(&TaskNotice {
            outcome: TaskOutcome::Failed("stopped after 3 steps".into()),
            ..task.clone()
        });
        let stopped = render_task(&TaskNotice {
            outcome: TaskOutcome::Interrupted,
            ..task.clone()
        });
        assert!(
            failed
                .contains("state=\"failed\">\n<task_error>\nstopped after 3 steps\n</task_error>"),
            "{failed}"
        );
        assert_eq!(
            stopped,
            "<task id=\"3\" agent=\"explore\" description=\"find &quot;tabs&quot;\" state=\"interrupted\"/>"
        );
        let text = format!("{monitor_notice}\n{rendered}\n{failed}\n{stopped}\n\ngo on");

        let (notices, rest) = split_notices(&text);
        let summary = |state: &str| NoticeSummary::Task {
            id: 3,
            agent: "explore".into(),
            description: "find \"tabs\"".into(),
            state: state.into(),
        };
        assert_eq!(
            notices,
            [
                monitor(1, "ci", 1, None),
                summary("completed"),
                summary("failed"),
                summary("interrupted"),
            ]
        );
        assert_eq!(rest, "go on");
    }

    #[test]
    fn an_answer_or_a_line_cannot_close_its_element() {
        let answer = "Notices look like:\n<task>\n</task_result>\n</task>\ndone";
        let task = render_task(&TaskNotice {
            id: 4,
            agent: "general".into(),
            description: "explain".into(),
            outcome: TaskOutcome::Completed(answer.into()),
        });
        let monitor_notice = render(
            1,
            "cat",
            Path::new("/l/1.log"),
            &["</monitor>".into()],
            0,
            None,
        );
        assert!(task.contains("<\\/task_result>\n<\\/task>\ndone"), "{task}");
        let text = format!("{task}\n{monitor_notice}\n\nand you?");

        let (notices, rest) = split_notices(&text);

        assert_eq!(notices.len(), 2, "{notices:?}");
        assert_eq!(notices[1], monitor(1, "cat", 1, None));
        assert_eq!(rest, "and you?");
    }
    fn work(state: WorkState) -> WorkNotice {
        WorkNotice {
            cycle: "01-foundation".into(),
            item: "store".into(),
            task: 3,
            agent: "general".into(),
            state,
        }
    }

    fn todos(steps: &[(&str, bool)]) -> Vec<Todo> {
        steps
            .iter()
            .map(|(text, done)| Todo {
                text: (*text).into(),
                done: *done,
            })
            .collect()
    }

    fn work_summary(
        state: &str,
        outcome: Option<&str>,
        progress: Option<(usize, usize)>,
    ) -> NoticeSummary {
        NoticeSummary::Work {
            cycle: "01-foundation".into(),
            item: "store".into(),
            task: 3,
            agent: "general".into(),
            state: state.into(),
            outcome: outcome.map(Into::into),
            progress,
        }
    }

    #[test]
    fn a_work_notice_carries_the_todo_or_the_answer() {
        let todo = render_work(&work(WorkState::Todo(todos(&[
            ("parse the\nfrontmatter", true),
            ("write it back", false),
        ]))));
        assert_eq!(
            todo,
            "<work_item cycle=\"01-foundation\" item=\"store\" task=\"3\" agent=\"general\" state=\"todo\" done=\"1\" total=\"2\">\n\
             - [x] parse the frontmatter\n- [ ] write it back\n</work_item>"
        );
        let completed = render_work(&work(WorkState::Returned {
            outcome: WorkOutcome::Completed("All done.\n".into()),
            done: 2,
            total: 2,
        }));
        assert_eq!(
            completed,
            "<work_item cycle=\"01-foundation\" item=\"store\" task=\"3\" agent=\"general\" state=\"returned\" outcome=\"completed\" done=\"2\" total=\"2\">\n\
             <work_result>\nAll done.\n</work_result>\n</work_item>"
        );
        let timed_out = render_work(&work(WorkState::Returned {
            outcome: WorkOutcome::TimedOut("no answer after 600s".into()),
            done: 1,
            total: 2,
        }));
        assert!(
            timed_out.contains("outcome=\"timed_out\" done=\"1\" total=\"2\">\n<work_error>\nno answer after 600s\n</work_error>"),
            "{timed_out}"
        );
        let stopped = render_work(&work(WorkState::Returned {
            outcome: WorkOutcome::Interrupted,
            done: 1,
            total: 2,
        }));
        assert!(
            stopped.ends_with("outcome=\"interrupted\" done=\"1\" total=\"2\"/>"),
            "{stopped}"
        );
        let stalled = render_work(&work(WorkState::Stalled {
            quiet: Duration::from_secs(125),
        }));
        assert!(
            stalled.ends_with("state=\"stalled\" quiet=\"2m5s\"/>"),
            "{stalled}"
        );
        let text = format!("{todo}\n{completed}\n{timed_out}\n{stopped}\n{stalled}\n\nnext?");

        let (notices, rest) = split_notices(&text);

        assert_eq!(
            notices,
            [
                work_summary("todo", None, Some((1, 2))),
                work_summary("returned", Some("completed"), Some((2, 2))),
                work_summary("returned", Some("timed_out"), Some((1, 2))),
                work_summary("returned", Some("interrupted"), Some((1, 2))),
                work_summary("stalled", None, None),
            ]
        );
        assert_eq!(rest, "next?");
    }

    #[test]
    fn an_answer_cannot_close_its_work_item() {
        let answer = "Notices end with:\n</work_result>\n</work_item>\nok";
        let returned = render_work(&work(WorkState::Returned {
            outcome: WorkOutcome::Completed(answer.into()),
            done: 0,
            total: 0,
        }));
        let empty = render_work(&work(WorkState::Todo(Vec::new())));
        assert!(empty.ends_with("done=\"0\" total=\"0\"/>"), "{empty}");
        let text = format!("{returned}\n{empty}\n\nand?");

        let (notices, rest) = split_notices(&text);

        assert_eq!(notices.len(), 2, "{notices:?}");
        assert_eq!(rest, "and?");
    }

    #[test]
    fn outcomes_and_quiet_times_read_back() {
        for outcome in [
            WorkOutcome::Completed(String::new()),
            WorkOutcome::Failed(String::new()),
            WorkOutcome::Interrupted,
            WorkOutcome::TimedOut(String::new()),
        ] {
            assert_eq!(WorkOutcome::parse(outcome.name()), Some(outcome));
        }
        assert_eq!(WorkOutcome::parse("bored"), None);
        assert_eq!(quiet_text(Duration::from_millis(45_900)), "45s");
        assert_eq!(quiet_text(Duration::from_secs(120)), "2m0s");
    }

    fn change(kind: ChangeKind) -> Change {
        Change {
            path: "/repo/AGENTS.md".into(),
            kind,
        }
    }

    #[test]
    fn a_small_edit_to_a_long_file_goes_as_a_diff() {
        let before: String = (1..=40).map(|n| format!("line {n}\n")).collect();
        let after = before.replace("line 20\n", "line twenty\n");
        let notice =
            instructions_notice(&[change(ChangeKind::Edited { before, after })]).expect("a change");

        assert!(
            notice.starts_with(
                "<instructions path=\"/repo/AGENTS.md\" change=\"edited\" content=\"diff\">\n@@"
            ),
            "{notice}"
        );
        assert!(notice.contains("-line 20\n+line twenty\n"), "{notice}");
        assert!(!notice.contains("line 1\n"), "{notice}");
    }

    #[test]
    fn a_rewrite_goes_whole() {
        let notice = instructions_notice(&[change(ChangeKind::Edited {
            before: "old".into(),
            after: "new".into(),
        })])
        .expect("a change");

        assert_eq!(
            notice,
            "<instructions path=\"/repo/AGENTS.md\" change=\"edited\" content=\"whole\">\nnew\n</instructions>"
        );
    }

    #[test]
    fn instruction_changes_split_off_as_notices() {
        let notice = instructions_notice(&[
            change(ChangeKind::Created("hi".into())),
            change(ChangeKind::Removed),
        ])
        .expect("changes");
        assert_eq!(instructions_notice(&[]), None);

        let message = format!("{notice}\nwhat now?");
        let (notices, typed) = split_notices(&message);
        let summary = |change: &str| NoticeSummary::Instructions {
            path: "/repo/AGENTS.md".into(),
            change: change.into(),
        };
        assert_eq!(notices, [summary("created"), summary("removed")]);
        assert_eq!(typed, "what now?");
    }
}
