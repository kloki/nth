//! The item file's markdown: frontmatter for its state, then a `# title`,
//! the brief, `## Todo` and `## Result`. Also how `plan.md` and a todo
//! list read and write.

use nth_context::frontmatter;

use crate::{Item, ItemRef, Status, Todo, WorkOutcome};

pub(crate) const TODO: &str = "## Todo";
pub(crate) const RESULT: &str = "## Result";

/// An item file as the store writes it.
pub fn render(item: &Item) -> String {
    let mut out = format!("---\nstatus: {}\n", item.status.name());
    if let Some(agent) = &item.agent {
        out.push_str(&format!("agent: {}\n", quoted(agent)));
    }
    if let Some(task) = item.task {
        out.push_str(&format!("task: {task}\n"));
    }
    if let Some(assigned) = item.assigned {
        out.push_str(&format!("assigned: {}\n", quoted(&assigned.to_string())));
    }
    if let Some(outcome) = &item.outcome {
        out.push_str(&format!("outcome: {}\n", outcome.name()));
    }
    if let Some(note) = &item.note {
        out.push_str(&format!("note: {}\n", quoted(&one_line(note))));
    }
    out.push_str("---\n\n");
    out.push_str(&item.body());
    out
}

/// A YAML double-quoted string, which takes any text on one line.
fn quoted(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

/// An item file. The outcome's text is the result's.
pub fn parse(at: ItemRef, text: &str) -> Result<Item, String> {
    let (front, body) = frontmatter::parse(text)?;
    let status = frontmatter::field(&front, "status").ok_or("no status in the frontmatter")?;
    let status = Status::parse(&status).ok_or_else(|| format!("unknown status {status:?}"))?;
    let assigned = match frontmatter::field(&front, "assigned") {
        Some(at) => Some(
            at.parse::<jiff::Timestamp>()
                .map_err(|e| format!("assigned {at:?} is not a time: {e}"))?,
        ),
        None => None,
    };
    // A hand edit may quote it.
    let task = frontmatter::number(&front, "task")
        .or_else(|| frontmatter::field(&front, "task")?.parse().ok());
    let task = match task {
        Some(task) => Some(u32::try_from(task).map_err(|_| format!("task {task} is too big"))?),
        None => None,
    };
    let sections = sections(body)?;
    let outcome = match frontmatter::field(&front, "outcome") {
        Some(name) => Some(
            with_text(&name, sections.result.as_deref().unwrap_or_default())
                .ok_or_else(|| format!("unknown outcome {name:?}"))?,
        ),
        None => None,
    };
    Ok(Item {
        at,
        title: sections.title,
        status,
        agent: frontmatter::field(&front, "agent"),
        task,
        assigned,
        outcome,
        note: frontmatter::field(&front, "note"),
        instruction: sections.instruction,
        todos: sections.todos,
        result: sections.result,
        modified: None,
    })
}

/// The outcome named `name`, carrying `text`.
fn with_text(name: &str, text: &str) -> Option<WorkOutcome> {
    Some(match WorkOutcome::parse(name)? {
        WorkOutcome::Completed(_) => WorkOutcome::Completed(text.to_string()),
        WorkOutcome::Failed(_) => WorkOutcome::Failed(text.to_string()),
        WorkOutcome::TimedOut(_) => WorkOutcome::TimedOut(text.to_string()),
        WorkOutcome::Interrupted => WorkOutcome::Interrupted,
    })
}

struct Sections {
    title: String,
    instruction: String,
    todos: Vec<Todo>,
    result: Option<String>,
}

/// The body: a `# title` line, the instruction, then `## Todo` and
/// `## Result`.
fn sections(body: &str) -> Result<Sections, String> {
    let mut lines = body.lines().skip_while(|line| line.trim().is_empty());
    let title = lines
        .next()
        .and_then(|line| line.strip_prefix("# "))
        .ok_or("the body does not start with a # title")?
        .trim()
        .to_string();
    enum Part {
        Instruction,
        Todo,
        Result,
    }
    let mut part = Part::Instruction;
    let (mut instruction, mut result) = (Vec::new(), Vec::new());
    let mut todos = Vec::new();
    for line in lines {
        match (&part, line.trim_end()) {
            // The result is last and the subagent's own words, so a heading
            // in it is just text.
            (Part::Result, _) => result.push(line),
            (_, TODO) => part = Part::Todo,
            (_, RESULT) => part = Part::Result,
            (Part::Instruction, line) => instruction.push(line),
            (Part::Todo, line) => todos.extend(todo(line)),
        }
    }
    let result = result.join("\n").trim().to_string();
    Ok(Sections {
        title,
        instruction: instruction.join("\n").trim().to_string(),
        todos,
        result: (!result.is_empty()).then_some(result),
    })
}

/// A `- [ ] step` or `- [x] step` line.
fn todo(line: &str) -> Option<Todo> {
    let rest = line.trim_start().strip_prefix("- [")?;
    let (mark, text) = rest.split_once("] ")?;
    let done = match mark {
        " " => false,
        "x" | "X" => true,
        _ => return None,
    };
    Some(Todo {
        text: text.trim().to_string(),
        done,
    })
}

/// Whether `instruction` has a line the body's sections would split at.
pub fn splits_sections(instruction: &str) -> bool {
    instruction
        .lines()
        .any(|line| matches!(line.trim_end(), TODO | RESULT))
}

/// `plan.md`'s first heading, and the rest.
pub(crate) fn overview(text: &str) -> (Option<String>, Option<String>) {
    let text = text.trim();
    let (title, goal) = match text.strip_prefix("# ") {
        Some(rest) => match rest.split_once('\n') {
            Some((title, goal)) => (Some(title.trim().to_string()), goal),
            None => (Some(rest.trim().to_string()), ""),
        },
        None => (None, text),
    };
    let goal = goal.trim();
    (title, (!goal.is_empty()).then(|| goal.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn at(cycle: &str, item: &str) -> ItemRef {
        ItemRef {
            cycle: cycle.into(),
            item: item.into(),
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

    #[test]
    fn an_item_reads_back_as_it_was_written() {
        let pending = Item::pending(
            at("01-foundation", "store"),
            "Store round-trip",
            "Read and write item files.\n\nUse `tokio::fs`.",
            todos(&[("parse", false), ("write", false)]),
        );
        let text = render(&pending);
        assert_eq!(
            text,
            "---\nstatus: pending\n---\n\n# Store round-trip\n\nRead and write item files.\n\nUse `tokio::fs`.\n\n\
             ## Todo\n\n- [ ] parse\n- [ ] write\n"
        );
        assert_eq!(parse(pending.at.clone(), &text), Ok(pending.clone()));

        let returned = Item {
            status: Status::Returned,
            agent: Some("general".into()),
            task: Some(3),
            assigned: Some("2026-10-11T12:02:11Z".parse().unwrap()),
            outcome: Some(WorkOutcome::Completed("Done: \"it\" works.".into())),
            note: Some("merged: in \"3b2c\"".into()),
            todos: todos(&[("parse", true), ("write", false)]),
            result: Some("Done: \"it\" works.".into()),
            ..pending
        };
        let text = render(&returned);
        assert!(
            text.starts_with("---\nstatus: returned\nagent: \"general\"\ntask: 3\nassigned: \"2026-10-11T12:02:11Z\"\noutcome: completed\nnote: \"merged: in \\\"3b2c\\\"\"\n---\n"),
            "{text}"
        );
        assert!(
            text.ends_with("- [x] parse\n- [ ] write\n\n## Result\n\nDone: \"it\" works.\n"),
            "{text}"
        );
        assert_eq!(parse(returned.at.clone(), &text), Ok(returned));
    }

    #[test]
    fn every_status_and_outcome_reads_back() {
        for status in [
            Status::Pending,
            Status::Running,
            Status::Returned,
            Status::Done,
            Status::Failed,
        ] {
            let item = Item {
                status: status.clone(),
                ..Item::pending(at("01-a", "x"), "X", "", Vec::new())
            };
            assert_eq!(
                parse(at("01-a", "x"), &render(&item)).map(|i| i.status),
                Ok(status)
            );
        }
        for outcome in [
            WorkOutcome::Failed("boom".into()),
            WorkOutcome::TimedOut("no answer after 1s".into()),
            WorkOutcome::Interrupted,
        ] {
            let item = Item {
                status: Status::Returned,
                result: Some(outcome.text().to_string()).filter(|t| !t.is_empty()),
                outcome: Some(outcome.clone()),
                ..Item::pending(at("01-a", "x"), "X", "", Vec::new())
            };
            assert_eq!(
                parse(at("01-a", "x"), &render(&item)).map(|i| i.outcome),
                Ok(Some(outcome))
            );
        }
    }

    #[test]
    fn hand_written_todos_and_sections_are_read_leniently() {
        let text = "---\nstatus: running\ntask: 2\n---\n# Tidy\n\nDo it.\n\n## Todo\n- [X] one\n\n- [ ] two\nnot a step\n";

        let item = parse(at("01-a", "tidy"), text).unwrap();

        assert_eq!(item.todos, todos(&[("one", true), ("two", false)]));
        assert_eq!(item.instruction, "Do it.");
        assert_eq!(item.task, Some(2));
        assert_eq!(item.result, None);
        assert!(splits_sections("a\n## Todo\nb"));
        assert!(!splits_sections("a\n### Todo\nb"));
    }

    #[test]
    fn a_result_keeps_its_own_headings() {
        let item = Item {
            status: Status::Returned,
            todos: todos(&[("one", true)]),
            result: Some(
                "Done. Follow-ups:\n\n## Todo\n\n- [ ] add tests\n\n## Result\n\nThat is all."
                    .into(),
            ),
            ..Item::pending(at("01-a", "x"), "X", "", Vec::new())
        };

        let back = parse(item.at.clone(), &render(&item)).unwrap();

        assert_eq!(back.todos, item.todos);
        assert_eq!(back.result, item.result);
        let quoted = parse(
            at("01-a", "x"),
            "---\nstatus: running\ntask: \"3\"\n---\n# X\n",
        )
        .unwrap();
        assert_eq!(quoted.task, Some(3));
    }

    #[test]
    fn a_file_that_is_not_an_item_says_why() {
        assert_eq!(
            parse(at("01-a", "x"), "---\nstatus: sleeping\n---\n# X\n"),
            Err("unknown status \"sleeping\"".into())
        );
        assert!(parse(at("01-a", "x"), "---\nstatus: done\n---\nno title").is_err());
        assert!(parse(at("01-a", "x"), "# X\n").is_err(), "no status");
    }
}
