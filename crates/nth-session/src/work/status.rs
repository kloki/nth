//! `work_status`: the plan as the model reads it, for when it resumes or
//! plans the next cycle.

use std::time::SystemTime;

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde_json::json;

use super::{Cycle, Item, Plan, Status, Work, reconciled};

pub(super) struct WorkStatus {
    work: Work,
}

impl WorkStatus {
    pub(super) fn new(work: Work) -> Self {
        Self { work }
    }
}

impl Tool for WorkStatus {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "work_status",
            description: include_str!("status.md").into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn call<'a>(&'a self, _: serde_json::Value, ctx: &'a ToolContext) -> BoxFuture<'a, ToolResult> {
        async move {
            let store = Work::store(ctx)?;
            let plan = {
                let live = self.work.files().0.lock().await;
                reconciled(&store, &live).await?
            };
            if plan.cycles.is_empty() && plan.ignored.is_empty() {
                return Ok(
                    "There is no work plan yet: write its first items with work_item.".into(),
                );
            }
            Ok(render(&plan, &store.dir().display().to_string()))
        }
        .boxed()
    }
}

fn render(plan: &Plan, dir: &str) -> String {
    let mut out = vec![format!("Work plan in {dir}")];
    if let Some(title) = &plan.title {
        out.push(format!("Title: {title}"));
    }
    if let Some(goal) = plan.goal.as_deref().and_then(|g| g.lines().next()) {
        out.push(format!("Goal: {goal}"));
    }
    let current = plan.first_open_cycle().map(Cycle::name);
    for cycle in &plan.cycles {
        let closed = cycle.items.iter().filter(|i| !i.status.is_open()).count();
        let state = match &current {
            _ if cycle.items.is_empty() => "empty".to_string(),
            _ if !cycle.is_open() => "closed".to_string(),
            Some(current) if *current == cycle.name() => "current".to_string(),
            Some(current) => format!("waits for {current}"),
            None => "closed".to_string(),
        };
        out.push(String::new());
        out.push(format!(
            "## {}: {state}, {closed} of {} closed",
            cycle.name(),
            cycle.items.len()
        ));
        out.extend(cycle.items.iter().map(line));
    }
    if !plan.ignored.is_empty() {
        out.push(String::new());
        out.push(format!(
            "Ignored folders, which the tools cannot reach (rename one to NN-slug, one folder a cycle number): {}",
            plan.ignored.join(", ")
        ));
    }
    out.join("\n")
}

/// `- store: running · @general task 3 · 3/7 ticked · assigned 4m ago`
fn line(item: &Item) -> String {
    let mut parts = vec![match (&item.status, &item.outcome) {
        (Status::Broken(why), _) => format!("broken: {why}"),
        (Status::Returned, Some(outcome)) => format!("returned ({outcome})"),
        (status, _) => status.to_string(),
    }];
    if let (Some(agent), Some(task)) = (&item.agent, item.task) {
        parts.push(format!("@{agent} task {task}"));
    }
    let (done, total) = item.progress();
    parts.push(format!("{done}/{total} ticked"));
    if matches!(item.status, Status::Running | Status::Returned) {
        if let Some(assigned) = item.assigned {
            let secs = jiff::Timestamp::now().as_second() - assigned.as_second();
            parts.push(format!("assigned {}", ago(secs.max(0) as u64)));
        }
        if let Some(modified) = item.modified {
            let secs = SystemTime::now()
                .duration_since(modified)
                .unwrap_or_default()
                .as_secs();
            parts.push(format!("last change {}", ago(secs)));
        }
    }
    if let Some(note) = &item.note {
        parts.push(format!("note: {note}"));
    }
    format!("- {}: {}", item.at.item, parts.join(" · "))
}

/// `20s ago`, `4m ago`, `2h ago`: one unit is enough to judge by.
fn ago(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s ago"),
        60..3600 => format!("{}m ago", secs / 60),
        _ => format!("{}h ago", secs / 3600),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{ItemRef, Store, item::WorkItem, tests::ctx, work_dir};

    #[tokio::test]
    async fn it_shows_each_cycle_and_item_with_where_it_stands() {
        let dir = tempfile::tempdir().unwrap();
        let work = crate::work::tests::work();
        let ctx = ctx(dir.path());
        let status = WorkStatus::new(work.clone());
        assert_eq!(
            status.call(json!({}), &ctx).await.unwrap(),
            "There is no work plan yet: write its first items with work_item."
        );
        let write = WorkItem::new(work);
        for (cycle, name, item) in [(1, "a", "x"), (1, "a", "y"), (2, "b", "z")] {
            let args = json!({ "cycle": cycle, "cycle_name": name, "item": item, "title": "T", "instruction": "I", "todos": ["one", "two"] });
            write.call(args, &ctx).await.unwrap();
        }
        let store = Store::new(work_dir(dir.path(), "s1"));
        let at = ItemRef {
            cycle: "01-a".into(),
            item: "x".into(),
        };
        let mut x = store.read(&at).await.unwrap().unwrap();
        x.status = Status::Done;
        x.note = Some("merged".into());
        x.agent = Some("general".into());
        x.task = Some(2);
        x.todos[0].done = true;
        store.write(&x).await.unwrap();
        std::fs::write(
            store.dir().join("plan.md"),
            "# Retry\n\nAdd retries.\nMore.",
        )
        .unwrap();
        std::fs::create_dir(store.dir().join("1-c")).unwrap();

        let text = status.call(json!({}), &ctx).await.unwrap();

        assert_eq!(
            text,
            format!(
                "Work plan in {}\nTitle: Retry\nGoal: Add retries.\n\n\
                 ## 01-a: current, 1 of 2 closed\n\
                 - x: done · @general task 2 · 1/2 ticked · note: merged\n\
                 - y: pending · 0/2 ticked\n\n\
                 ## 02-b: waits for 01-a, 0 of 1 closed\n\
                 - z: pending · 0/2 ticked\n\n\
                 Ignored folders, which the tools cannot reach (rename one to NN-slug, one folder a cycle number): 1-c",
                store.dir().display()
            )
        );
    }

    #[test]
    fn ago_takes_one_unit() {
        assert_eq!(ago(59), "59s ago");
        assert_eq!(ago(61), "1m ago");
        assert_eq!(ago(7300), "2h ago");
    }
}
