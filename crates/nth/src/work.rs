//! `nth work`: a saved session's work plan as the TUI's work tab draws it,
//! the latest session by default. Nothing runs here, so a running item
//! shows as its file says.

use std::io::{IsTerminal, Write};

use anyhow::{Result, bail};
use nth_icons::icons;
use nth_session::{
    Session, Store,
    work::{
        self, Item, Plan, Status,
        outline::{CycleState, Guide, Kind, Row},
    },
};
use owo_colors::OwoColorize;
use serde_json::{Value, json};

pub async fn run(session: Option<String>, json: bool) -> Result<()> {
    let store = Store::open()?;
    let session = crate::usage::pick(&store, session.as_deref()).await?;
    let Some(plan) = work::Store::new(session.work_dir()).load().await? else {
        bail!(
            "session {} has no work plan in {}",
            session.id,
            session.work_dir().display()
        );
    };
    let mut out = std::io::stdout().lock();
    match json || !out.is_terminal() {
        true => writeln!(out, "{}", plan_json(&session, &plan))?,
        false => text(&mut out, &session, &plan)?,
    }
    Ok(())
}

fn text(out: &mut impl Write, session: &Session, plan: &Plan) -> Result<()> {
    let outline = plan.outline();
    let title = plan
        .title
        .clone()
        .or_else(|| session.title())
        .unwrap_or_default();
    writeln!(
        out,
        "{} {}  {}",
        icons().to.cyan().bold(),
        title.bold(),
        session.id.to_string().dimmed()
    )?;
    let mut counts = format!("{}/{} done", outline.done, outline.total);
    for (count, what) in [
        (outline.running, "running"),
        (outline.returned, "returned"),
        (outline.failed, "failed"),
    ] {
        if count > 0 {
            counts.push_str(&format!(" · {count} {what}"));
        }
    }
    writeln!(out, "  {}", counts.dimmed())?;
    for row in &outline.rows {
        match &row.kind {
            Kind::Title(_) => {}
            Kind::Goal(goal) => writeln!(out, "  {}", goal.replace('\n', " ").dimmed())?,
            kind => {
                if matches!(kind, Kind::Cycle { .. }) {
                    writeln!(out)?;
                }
                writeln!(out, "  {}{}", guides(row).dimmed(), paint(kind))?;
            }
        }
    }
    Ok(())
}

/// The tree's guides in front of a row, as the work tab draws them.
fn guides(row: &Row) -> String {
    let tree = &icons().tree;
    let mut out: String = row
        .guides
        .iter()
        .map(|guide| match guide {
            Guide::Bar => format!("{}  ", tree.bar),
            Guide::Blank => "   ".to_string(),
            Guide::Tee => format!("{}{} ", tree.tee, tree.dash),
            Guide::Elbow => format!("{}{} ", tree.elbow, tree.dash),
        })
        .collect();
    if matches!(row.kind, Kind::Todo { .. } | Kind::Note(_)) {
        out.push_str("  ");
    }
    out
}

/// One row, coloured as the tab colours it.
fn paint(kind: &Kind) -> String {
    let icons = icons();
    match kind {
        Kind::Title(title) => title.bold().to_string(),
        Kind::Goal(goal) => goal.dimmed().to_string(),
        Kind::Cycle {
            number,
            slug,
            state,
            closed,
            total,
        } => {
            let name = format!("{number} {slug}");
            let (icon, name) = match state {
                CycleState::Closed => (icons.ok.green().to_string(), name),
                CycleState::Current => (icons.current.blue().to_string(), name.bold().to_string()),
                CycleState::Upcoming => (
                    icons.work.upcoming.dimmed().to_string(),
                    name.dimmed().to_string(),
                ),
            };
            format!("{icon} {name}  {}", format!("{closed}/{total}").dimmed())
        }
        Kind::Item(item) => item_text(item),
        Kind::Todo { text, done: true } => {
            format!("{} {}", icons.checked.green(), text.dimmed())
        }
        Kind::Todo { text, done: false } => format!("{} {text}", icons.unchecked.dimmed()),
        Kind::Note(note) => format!("note: {note}").dimmed().to_string(),
    }
}

fn item_text(item: &Item) -> String {
    let icons = icons();
    let icon = match &item.status {
        Status::Pending => icons.work.pending.dimmed().to_string(),
        Status::Running => icons.dot.yellow().to_string(),
        Status::Returned => icons.done.magenta().to_string(),
        Status::Done => icons.ok.green().to_string(),
        Status::Failed | Status::Broken(_) => icons.fail.red().to_string(),
    };
    let name = match item.status {
        Status::Pending => item.at.item.dimmed().to_string(),
        _ => item.at.item.clone(),
    };
    let mut out = format!("{icon} {name}");
    let (done, total) = item.progress();
    if total > 0 {
        out.push_str(&format!("  {}", format!("{done}/{total}").dimmed()));
    }
    if matches!(
        item.status,
        Status::Running | Status::Returned | Status::Failed
    ) && let (Some(agent), Some(task)) = (&item.agent, item.task)
    {
        out.push_str(&format!(
            "  {} {}",
            format!("@{agent}").cyan(),
            format!("#{task}").dimmed()
        ));
    }
    match &item.status {
        Status::Running => out.push_str(&" · running".yellow().to_string()),
        Status::Returned => {
            let outcome = item
                .outcome
                .as_ref()
                .map(|o| format!(" {}", o.name()))
                .unwrap_or_default();
            out.push_str(&format!(" · returned{outcome}").magenta().to_string());
        }
        Status::Failed => out.push_str(&" · failed".red().to_string()),
        Status::Broken(why) => out.push_str(&format!(" · {why}").red().to_string()),
        Status::Pending | Status::Done => {}
    }
    out
}

fn plan_json(session: &Session, plan: &Plan) -> Value {
    let outline = plan.outline();
    let current = plan.first_open_cycle().map(|cycle| cycle.number);
    let cycles: Vec<Value> = plan
        .cycles
        .iter()
        .map(|cycle| {
            let state = match current {
                Some(n) if n == cycle.number => "current",
                Some(n) if n < cycle.number => "upcoming",
                _ => "closed",
            };
            json!({
                "number": cycle.number,
                "slug": cycle.slug,
                "state": state,
                "items": cycle.items.iter().map(item_json).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "id": session.id,
        "dir": session.work_dir(),
        "title": plan.title,
        "goal": plan.goal,
        "done": outline.done,
        "total": outline.total,
        "cycles": cycles,
    })
}

fn item_json(item: &Item) -> Value {
    let (done, total) = item.progress();
    let todos: Vec<Value> = item
        .todos
        .iter()
        .map(|todo| json!({ "text": todo.text, "done": todo.done }))
        .collect();
    let broken = match &item.status {
        Status::Broken(why) => Some(why.clone()),
        _ => None,
    };
    json!({
        "item": item.at.item,
        "title": item.title,
        "status": item.status.name(),
        "broken": broken,
        "agent": item.agent,
        "task": item.task,
        "assigned": item.assigned.map(|at| at.to_string()),
        "outcome": item.outcome.as_ref().map(|o| o.name()),
        "done": done,
        "total": total,
        "todos": todos,
        "note": item.note,
        "result": item.result,
    })
}

#[cfg(test)]
mod tests {
    use nth_protocol::{Todo, WorkOutcome};
    use nth_session::work::{Cycle, ItemRef};

    use super::*;

    fn plan() -> Plan {
        let at = |cycle: &str, item: &str| ItemRef {
            cycle: cycle.into(),
            item: item.into(),
        };
        let todos = vec![
            Todo {
                text: "parse".into(),
                done: true,
            },
            Todo {
                text: "write".into(),
                done: false,
            },
        ];
        Plan {
            title: Some("Retry".into()),
            ignored: Vec::new(),
            goal: Some("Add retries.".into()),
            cycles: vec![
                Cycle {
                    number: 1,
                    slug: "a".into(),
                    items: vec![Item {
                        status: Status::Returned,
                        agent: Some("general".into()),
                        task: Some(2),
                        outcome: Some(WorkOutcome::Completed("ok".into())),
                        result: Some("ok".into()),
                        ..Item::pending(at("01-a", "store"), "Store", "Do it.", todos.clone())
                    }],
                },
                Cycle {
                    number: 2,
                    slug: "b".into(),
                    items: vec![Item::pending(at("02-b", "tab"), "Tab", "", todos)],
                },
            ],
        }
    }

    /// The text without its colours.
    fn plain(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                chars.by_ref().find(|c| *c == 'm');
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn the_text_draws_the_tree() {
        let session = Session::new("glm", "/repo".into());
        let mut out = Vec::new();

        text(&mut out, &session, &plan()).unwrap();

        let lines: Vec<String> = plain(&String::from_utf8(out).unwrap())
            .lines()
            .map(String::from)
            .collect();
        assert_eq!(lines[0], format!("→ Retry  {}", session.id));
        assert_eq!(
            lines[1..],
            [
                "  0/2 done · 1 returned",
                "  Add retries.",
                "",
                "  ▸ 1 a  0/1",
                "  └─ ∎ store  1/2  @general #2 · returned completed",
                "       ☒ parse",
                "       ☐ write",
                "",
                "  ◇ 2 b  0/1",
                "  └─ ○ tab  1/2",
            ]
        );
    }

    #[test]
    fn the_json_has_cycles_items_and_their_todos() {
        let session = Session::new("glm", "/repo".into());

        let value = plan_json(&session, &plan());

        assert_eq!(value["title"], "Retry");
        assert_eq!(
            (value["done"].clone(), value["total"].clone()),
            (json!(0), json!(2))
        );
        assert_eq!(value["cycles"][0]["state"], "current");
        assert_eq!(value["cycles"][1]["state"], "upcoming");
        let store = &value["cycles"][0]["items"][0];
        assert_eq!(store["status"], "returned");
        assert_eq!(store["outcome"], "completed");
        assert_eq!(store["task"], 2);
        assert_eq!(store["todos"][0], json!({ "text": "parse", "done": true }));
        assert_eq!(value["dir"], json!(session.work_dir()));
    }
}
