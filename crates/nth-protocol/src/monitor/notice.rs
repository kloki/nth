//! The notices the model reads between its steps: a `<monitor>` element for
//! what a monitor said, a `<task>` element for what a subagent answered, and
//! what the transcript reads back out of them. Both sides are here so they
//! cannot drift apart.

use std::{fmt, path::Path};

use super::{MonitorEnd, MonitorId};

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
}

/// Splits the notices off the front of a user message: what monitors and
/// subagents said, then what the user typed after it, if anything.
pub fn split_notices(text: &str) -> (Vec<NoticeSummary>, &str) {
    let mut notices = Vec::new();
    let mut rest = text;
    while let Some((element, after)) = ["monitor", "task"]
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
        let Some(id) = value(attrs, "id").and_then(|id| id.parse().ok()) else {
            break;
        };
        let description = value(attrs, "description").unwrap_or_default();
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
        notices.push(match element {
            "monitor" => NoticeSummary::Monitor {
                id,
                description,
                lines: inner.lines().filter(|l| !l.is_empty()).count(),
                ended: value(attrs, "ended"),
            },
            _ => NoticeSummary::Task {
                id,
                agent: value(attrs, "agent").unwrap_or_default(),
                description,
                state: value(attrs, "state").unwrap_or_default(),
            },
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
}
