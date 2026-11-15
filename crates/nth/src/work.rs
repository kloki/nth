//! `nth work`: a saved session's work plan as the TUI's work tab draws it,
//! the latest session by default. Nothing runs here, so a running item
//! shows as its file says.

use std::io::Write;

use anyhow::{Result, bail};
use nth_icons::icons;
use nth_session::{Session, Store};
use nth_work::{
    Item, Plan, Status,
    outline::{CycleState, Guide, Kind, Row},
};
use owo_colors::OwoColorize;
use serde_json::{Value, json};

use crate::output::{self, Report};

pub async fn run(session: Option<String>, json: bool) -> Result<()> {
    let store = Store::open()?;
    let session = crate::session::pick(&store, session.as_deref()).await?;
    let Some(plan) = nth_work::Store::new(session.work_dir()).load().await? else {
        bail!(
            "session {} has no work plan in {}",
            session.id,
            session.work_dir().display()
        );
    };
    output::print(&WorkReport { session, plan }, json)
}

/// A session's work plan, as `nth work` prints it.
struct WorkReport {
    session: Session,
    plan: Plan,
}

impl Report for WorkReport {
    fn text(&self, out: &mut dyn Write) -> Result<()> {
        text(out, &self.session, &self.plan)
    }

    fn json(&self, out: &mut dyn Write) -> Result<()> {
        writeln!(out, "{}", plan_json(&self.session, &self.plan))?;
        Ok(())
    }
}

fn text(out: &mut dyn Write, session: &Session, plan: &Plan) -> Result<()> {
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
    let models: Vec<String> = plan
        .models()
        .into_iter()
        .map(|(model, count)| format!("{} {}", model_text(model, None), count.dimmed()))
        .collect();
    if !models.is_empty() {
        writeln!(out, "  {}", models.join(&" · ".dimmed().to_string()))?;
    }
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
    out.push_str(&worker_text(item));
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

/// `  Atlas @general #3 · zen/glm · high`, as the work tab draws it: the
/// parts the item's file has, coloured while the item is in play and all
/// dim once it is done or back to pending.
fn worker_text(item: &Item) -> String {
    let who: Vec<String> = [
        item.name.clone(),
        item.agent.as_ref().map(|agent| format!("@{agent}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    let task = item.task.map(|task| format!("#{task}"));
    let in_play = matches!(
        item.status,
        Status::Running | Status::Returned | Status::Failed
    );
    if !in_play {
        // Plain text dimmed once: a part with a style of its own would
        // end in a reset, and the rest of the row would lose its dim.
        let mut parts: Vec<String> = Vec::new();
        let who: Vec<String> = who.into_iter().chain(task).collect();
        if !who.is_empty() {
            parts.push(who.join(" "));
        }
        if let Some(model) = &item.model {
            parts.push(match &item.effort {
                Some(effort) => format!("{model} · {effort}"),
                None => model.clone(),
            });
        }
        return match parts.is_empty() {
            true => String::new(),
            false => format!("  {}", parts.join(" · ").dimmed()),
        };
    }
    let who: Vec<String> = who
        .into_iter()
        .map(|part| part.cyan().to_string())
        .chain(task.map(|task| task.dimmed().to_string()))
        .collect();
    let mut parts: Vec<String> = Vec::new();
    if !who.is_empty() {
        parts.push(who.join(" "));
    }
    if let Some(model) = &item.model {
        parts.push(model_text(model, item.effort.as_deref()));
    }
    match parts.is_empty() {
        true => String::new(),
        false => format!("  {}", parts.join(&" · ".dimmed().to_string())),
    }
}

/// A model in the TUI's colours: the provider bright white, the id blue,
/// the effort grey.
fn model_text(model: &str, effort: Option<&str>) -> String {
    let mut out = match model
        .split_once('/')
        .filter(|(provider, _)| !provider.is_empty())
    {
        Some((provider, id)) => {
            format!("{}{}", format!("{provider}/").bright_white(), id.blue())
        }
        None => model.blue().to_string(),
    };
    if let Some(effort) = effort {
        out.push_str(&format!(" · {effort}").white().to_string());
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
        "name": item.name,
        "model": item.model,
        "effort": item.effort,
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
    use nth_work::{Cycle, ItemRef, Todo, WorkOutcome};

    use super::*;
    use crate::output::testing;

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
                        name: Some("Atlas".into()),
                        model: Some("zen/glm".into()),
                        effort: Some("high".into()),
                        outcome: Some(WorkOutcome::Completed("ok".into())),
                        result: Some("ok".into()),
                        ..Item::pending(at("01-a", "store"), "Store", "Do it.", todos.clone())
                    }],
                },
                Cycle {
                    number: 2,
                    slug: "b".into(),
                    items: vec![Item {
                        // Sent back to pending by a file from before
                        // names and models were kept.
                        agent: Some("explore".into()),
                        task: Some(1),
                        ..Item::pending(at("02-b", "tab"), "Tab", "", todos)
                    }],
                },
            ],
        }
    }

    /// A session with an id a snapshot can hold.
    fn session() -> Session {
        let mut session = Session::new("glm", "/repo".into());
        session.id = "0a1b2c3d-0000-4000-8000-000000000000"
            .parse()
            .expect("a uuid");
        session
    }

    #[test]
    fn a_plan_draws_as_a_tree_of_cycles_items_and_todos() {
        let report = WorkReport {
            session: session(),
            plan: plan(),
        };

        let text = testing::text(&report);

        insta::assert_snapshot!(text);
    }

    #[test]
    fn a_plan_is_one_json_document() {
        let report = WorkReport {
            session: session(),
            plan: plan(),
        };

        let value: Value = serde_json::from_str(&testing::json(&report)).unwrap();

        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn the_text_draws_the_tree() {
        let session = Session::new("glm", "/repo".into());
        let id = session.id;
        let report = WorkReport {
            session,
            plan: plan(),
        };

        let lines: Vec<String> = testing::text(&report).lines().map(String::from).collect();
        assert_eq!(lines[0], format!("→ Retry  {id}"));
        assert_eq!(
            lines[1..],
            [
                "  0/2 done · 1 returned",
                "  zen/glm 1",
                "  Add retries.",
                "",
                "  → 1 a  0/1",
                "  └─ ∎ store  1/2  Atlas @general #2 · zen/glm · high · returned completed",
                "       ☒ parse",
                "       ☐ write",
                "",
                "  ● 2 b  0/1",
                "  └─ ● tab  1/2  @explore #1",
            ]
        );
    }

    #[test]
    fn the_counts_are_followed_by_the_items_of_each_model() {
        let session = Session::new("glm", "/repo".into());
        let mut plan = plan();
        plan.cycles[1].items[0].model = Some("chatgpt/gpt-5".into());
        let again = plan.cycles[1].items[0].clone();
        plan.cycles[1].items.push(again);

        let text = testing::text(&WorkReport { session, plan });

        assert_eq!(text.lines().nth(2), Some("  chatgpt/gpt-5 2 · zen/glm 1"));
    }

    #[test]
    fn a_finished_items_worker_is_dim_to_its_end_and_one_in_play_coloured() {
        let mut item = plan().cycles[0].items[0].clone();

        // Returned: the name cyan, the model in its own colours.
        let in_play = worker_text(&item);
        assert!(in_play.contains(&"Atlas".cyan().to_string()), "{in_play:?}");
        assert!(in_play.contains(&"glm".blue().to_string()), "{in_play:?}");

        // Done: one dim run from the name to the effort, nothing inside
        // it ending the dim early.
        item.status = Status::Done;
        assert_eq!(
            worker_text(&item),
            format!("  {}", "Atlas @general #2 · zen/glm · high".dimmed())
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
