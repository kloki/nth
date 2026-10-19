//! `work_close`: the orchestrator's verdict on an item, done or failed.

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec, one_line};
use serde::Deserialize;
use serde_json::json;

use super::{ItemRef, Plan, Status, Work, open_items};

pub(super) struct WorkClose {
    work: Work,
}

impl WorkClose {
    pub(super) fn new(work: Work) -> Self {
        Self { work }
    }
}

#[derive(Deserialize)]
struct Args {
    cycle: u32,
    item: String,
    outcome: Outcome,
    note: Option<String>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "lowercase")]
enum Outcome {
    Done,
    Failed,
}

impl Tool for WorkClose {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "work_close",
            description: include_str!("close.md").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "cycle": { "type": "integer", "description": "The item's cycle number." },
                    "item": { "type": "string", "description": "The item's slug." },
                    "outcome": { "type": "string", "enum": ["done", "failed"], "description": "Your verdict." },
                    "note": { "type": "string", "description": "One line: what you verified, or why it failed." }
                },
                "required": ["cycle", "item", "outcome"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            let args: Args = nth_protocol::parse_args(args)?;
            let store = Work::store(ctx)?;
            let (_live, mut plan) = self.work.locked_plan(&store).await?;
            let Some(cycle) = plan.cycle(args.cycle) else {
                return Err(format!("there is no cycle {}", args.cycle));
            };
            let at = ItemRef {
                cycle: cycle.name(),
                item: args.item,
            };
            let Some(item) = plan.item(&at) else {
                return Err(format!("there is no item {at}"));
            };
            let mut item = item.clone();
            match &item.status {
                Status::Running => {
                    let task = item.task.map(|t| format!(" (task {t})")).unwrap_or_default();
                    return Err(format!(
                        "{at} is running{task}; let it return, or stop it with task_stop, then close it"
                    ));
                }
                Status::Broken(why) => {
                    return Err(format!(
                        "{at} does not read as an item ({why}); write it again with work_item"
                    ));
                }
                _ => {}
            }
            let status = match args.outcome {
                Outcome::Done => Status::Done,
                Outcome::Failed => Status::Failed,
            };
            // A second verdict that says the same is a model that lost
            // track; a different one is a change of mind, which stands.
            if item.status == status {
                return Err(format!(
                    "{at} is closed as {status} already; work_item writes it again if there is more to do"
                ));
            }
            item.status = status;
            item.note = args
                .note
                .map(|note| one_line(&note))
                .filter(|note| !note.is_empty());
            store
                .write(&item)
                .await
                .map_err(|e| format!("writing {at} failed: {e}"))?;
            let status = item.status.clone();
            if let Some(slot) = plan
                .cycles
                .iter_mut()
                .flat_map(|cycle| &mut cycle.items)
                .find(|i| i.at == at)
            {
                *slot = item;
            }
            Ok(format!("Closed {at} as {status}. {}", next(&plan)))
        }
        .boxed()
    }
}

/// What the plan waits for now.
fn next(plan: &Plan) -> String {
    match plan.first_open_cycle() {
        None => "Every item of every cycle is closed: the plan is complete.".into(),
        Some(cycle) if cycle.items.iter().all(|i| i.status == Status::Pending) => {
            format!("Cycle {} is next: assign its items.", cycle.name())
        }
        Some(cycle) => format!(
            "Cycle {} still has open items: {}.",
            cycle.name(),
            open_items(cycle)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{Item, Store, item::WorkItem, tests::ctx, work_dir};

    #[tokio::test]
    async fn closing_says_what_the_plan_waits_for() {
        let dir = tempfile::tempdir().unwrap();
        let work = crate::work::tests::work();
        let ctx = ctx(dir.path());
        let write = WorkItem::new(work.clone());
        for (cycle, name, item) in [(1, "a", "x"), (1, "a", "y"), (2, "b", "z")] {
            let args = json!({ "cycle": cycle, "cycle_name": name, "item": item, "title": "T", "instruction": "I", "todos": [] });
            write.call(args, &ctx).await.unwrap();
        }
        let close = WorkClose::new(work);
        let args = |cycle: u32, item: &str, outcome: &str| json!({ "cycle": cycle, "item": item, "outcome": outcome, "note": "checked\nby hand" });

        assert_eq!(
            close.call(args(1, "x", "done"), &ctx).await.unwrap(),
            "Closed 01-a/x as done. Cycle 01-a still has open items: y (pending)."
        );
        assert_eq!(
            close.call(args(1, "y", "failed"), &ctx).await.unwrap(),
            "Closed 01-a/y as failed. Cycle 02-b is next: assign its items."
        );
        assert_eq!(
            close.call(args(2, "z", "done"), &ctx).await.unwrap(),
            "Closed 02-b/z as done. Every item of every cycle is closed: the plan is complete."
        );
        let store = Store::new(work_dir(dir.path(), "s1"));
        let at = ItemRef {
            cycle: "01-a".into(),
            item: "x".into(),
        };
        let item = store.read(&at).await.unwrap().unwrap();
        assert_eq!(item.note.as_deref(), Some("checked by hand"));
        assert_eq!(
            close.call(args(1, "x", "done"), &ctx).await.unwrap_err(),
            "01-a/x is closed as done already; work_item writes it again if there is more to do"
        );
        assert_eq!(
            close.call(args(3, "x", "done"), &ctx).await.unwrap_err(),
            "there is no cycle 3"
        );
        assert_eq!(
            close.call(args(1, "w", "done"), &ctx).await.unwrap_err(),
            "there is no item 01-a/w"
        );
        store
            .write(&Item {
                status: Status::Running,
                ..item
            })
            .await
            .unwrap();
        assert!(
            close
                .call(args(1, "x", "done"), &ctx)
                .await
                .unwrap()
                .contains("as done"),
            "nobody runs it in this process, so it was settled first"
        );
    }
}
