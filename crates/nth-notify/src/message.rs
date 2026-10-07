//! The text of each notification, from the files in `templates/`: the first
//! line is the summary, the rest the body, with `{name}` filled in.

use std::time::Duration;

use crate::{Notification, Urgency};

const DONE: &str = include_str!("../templates/done.txt");
const PLAN_READY: &str = include_str!("../templates/plan_ready.txt");
const FAILED: &str = include_str!("../templates/failed.txt");
const NEEDS_YOU: &str = include_str!("../templates/needs_you.txt");

/// Of the model's reply or an error: a glance, not the whole text.
const MAX_LINES: usize = 3;
const MAX_CHARS: usize = 200;

/// What happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The model answered and nth waits for the next prompt.
    Done,
    /// As `Done`, in plan mode with a plan to approve.
    PlanReady,
    Failed(String),
    /// A question waits for an answer.
    NeedsYou {
        header: String,
        question: String,
        /// Questions after this one in the same ask.
        more: usize,
    },
}

/// Which of your sessions it is about, and how its turn went.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    /// The session's title: its first prompt.
    pub title: String,
    /// How long the turn ran.
    pub elapsed: Duration,
    /// The model's last answer.
    pub reply: Option<String>,
}

impl Event {
    pub fn notification(&self, cx: &Context) -> Notification {
        let elapsed = elapsed(cx.elapsed);
        let title = clip(&cx.title);
        let mut values = vec![("title", title), ("elapsed", elapsed)];
        let (template, urgency) = match self {
            Event::Done => {
                values.push(("reply", cx.reply.as_deref().map(clip).unwrap_or_default()));
                (DONE, Urgency::Normal)
            }
            Event::PlanReady => (PLAN_READY, Urgency::Normal),
            Event::Failed(error) => {
                values.push(("error", clip(error)));
                (FAILED, Urgency::Critical)
            }
            Event::NeedsYou {
                header,
                question,
                more,
            } => {
                values.push(("header", header.clone()));
                values.push(("question", clip(question)));
                values.push((
                    "more",
                    match more {
                        0 => String::new(),
                        1 => "and 1 more question".into(),
                        n => format!("and {n} more questions"),
                    },
                ));
                (NEEDS_YOU, Urgency::Critical)
            }
        };
        let text = fill(template, &values);
        let (summary, body) = text.split_once('\n').unwrap_or((&text, ""));
        Notification {
            summary: summary.to_string(),
            // A value left empty leaves its line empty.
            body: body
                .lines()
                .filter(|line| !line.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n"),
            urgency,
        }
    }
}

/// Replaces each `{name}` in one pass, so a value holding `{x}` stays as it
/// is. An unknown name is left as written.
fn fill(template: &str, values: &[(&str, String)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let value = after.find('}').and_then(|close| {
            let name = &after[..close];
            let (_, value) = values.iter().find(|(key, _)| *key == name)?;
            Some((value, close))
        });
        match value {
            Some((value, close)) => {
                out.push_str(value);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The first `MAX_LINES` non-empty lines, at most `MAX_CHARS` characters.
fn clip(text: &str) -> String {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.trim().is_empty())
        .collect();
    let mut clipped = lines[..lines.len().min(MAX_LINES)].join("\n");
    let mut cut = lines.len() > MAX_LINES;
    if let Some((at, _)) = clipped.char_indices().nth(MAX_CHARS) {
        clipped.truncate(at);
        cut = true;
    }
    if cut {
        clipped.push('…');
    }
    clipped
}

/// As in `2m 14s`.
fn elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match (secs / 3600, secs / 60 % 60, secs % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, s) => format!("{m}m {s}s"),
        (h, m, _) => format!("{h}h {m}m"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx() -> Context {
        Context {
            title: "add notification support".into(),
            elapsed: Duration::from_secs(134),
            reply: Some("# Done\n\nAdded `nth-notify`.\nIt uses {title}.\nline 4".into()),
        }
    }

    #[test]
    fn done_says_where_how_long_and_how_it_ended() {
        let n = Event::Done.notification(&cx());
        assert_eq!(n.summary, "nth · done in 2m 14s");
        assert_eq!(
            n.body,
            "add notification support\n# Done\nAdded `nth-notify`.\nIt uses {title}.…"
        );
        assert_eq!(n.urgency, Urgency::Normal);
    }

    #[test]
    fn done_without_a_reply_has_no_empty_line() {
        let cx = Context {
            reply: None,
            ..cx()
        };
        assert_eq!(
            Event::Done.notification(&cx).body,
            "add notification support"
        );
    }

    #[test]
    fn plan_ready_points_at_approve() {
        let n = Event::PlanReady.notification(&cx());
        assert_eq!(n.summary, "nth · plan ready");
        assert_eq!(
            n.body,
            "add notification support\nReview the Plan tab, then /approve."
        );
    }

    #[test]
    fn failed_is_critical_with_the_error() {
        let n = Event::Failed("rate limited".into()).notification(&cx());
        assert_eq!(n.summary, "nth · failed after 2m 14s");
        assert_eq!(n.body, "add notification support\nrate limited");
        assert_eq!(n.urgency, Urgency::Critical);
    }

    #[test]
    fn needs_you_leads_with_the_question() {
        let event = Event::NeedsYou {
            header: "Doom loop".into(),
            question: "Keep going?".into(),
            more: 2,
        };
        let n = event.notification(&cx());
        assert_eq!(n.summary, "nth · needs you");
        assert_eq!(
            n.body,
            "Doom loop: Keep going?\nand 2 more questions\nadd notification support"
        );
        assert_eq!(n.urgency, Urgency::Critical);
    }

    #[test]
    fn clips_long_text_on_a_char_boundary() {
        let clipped = clip(&"é".repeat(300));
        assert_eq!(clipped.chars().count(), MAX_CHARS + 1);
        assert!(clipped.ends_with('…'));
    }

    #[test]
    fn elapsed_reads_like_a_clock() {
        assert_eq!(elapsed(Duration::from_secs(9)), "9s");
        assert_eq!(elapsed(Duration::from_secs(3 * 3600 + 120)), "3h 2m");
    }
}
