//! `work_item`: writes an item of the plan, new or again.

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Todo, Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use super::{Item, ItemRef, Status, Work, cycle_name, reconciled, slug, store::splits_sections};
use crate::plan;

pub(super) struct WorkItem {
    work: Work,
}

impl WorkItem {
    pub(super) fn new(work: Work) -> Self {
        Self { work }
    }
}

#[derive(Deserialize)]
struct Args {
    cycle: u32,
    cycle_name: Option<String>,
    item: String,
    title: String,
    instruction: String,
    #[serde(default)]
    todos: Vec<String>,
}

impl Tool for WorkItem {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "work_item",
            description: include_str!("item.md").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "cycle": { "type": "integer", "minimum": 1, "description": "The cycle the item belongs to. Cycles run one after another, lowest number first; the items of one cycle run in parallel." },
                    "cycle_name": { "type": "string", "description": "A slug naming the cycle (lowercase, digits, hyphens). Needed the first time a cycle number is used; leave it out after." },
                    "item": { "type": "string", "description": "The item's slug, unique in its cycle (lowercase, digits, hyphens)." },
                    "title": { "type": "string", "description": "One line naming the work." },
                    "instruction": { "type": "string", "description": "Markdown for the subagent that will do it: what to do, where, how to verify it, and what to report back. It sees nothing else of the plan." },
                    "todos": { "type": "array", "items": { "type": "string" }, "description": "The steps, in order, as the subagent should tick them off." }
                },
                "required": ["cycle", "item", "title", "instruction", "todos"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            let args: Args = crate::agent_loop::parse_args(args)?;
            let store = Work::store(ctx)?;
            slug(&args.item)?;
            if let Some(name) = &args.cycle_name {
                slug(name)?;
            }
            if args.cycle == 0 {
                return Err("cycles are numbered from 1".into());
            }
            if args.title.trim().is_empty() {
                return Err("the title is empty".into());
            }
            if splits_sections(&args.instruction) {
                return Err("the instruction may not have a line \"## Todo\" or \"## Result\": the item file has those sections of its own".into());
            }
            let live = self.work.files().0.lock().await;
            let plan = reconciled(&store, &live).await?;
            let cycle = plan.cycle(args.cycle);
            let folder = match (cycle, &args.cycle_name) {
                (Some(cycle), Some(name)) if *name != cycle.slug => {
                    return Err(format!(
                        "cycle {} is {}; leave cycle_name out",
                        args.cycle,
                        cycle.name()
                    ));
                }
                (Some(cycle), _) => cycle.name(),
                (None, Some(name)) => cycle_name(args.cycle, name),
                (None, None) => {
                    return Err(format!(
                        "cycle {} does not exist yet; give cycle_name to start it",
                        args.cycle
                    ));
                }
            };
            let at = ItemRef {
                cycle: folder,
                item: args.item,
            };
            let before = plan.item(&at);
            if let Some(Item {
                status: Status::Running,
                task,
                ..
            }) = before
            {
                let task = task.map(|t| format!(" (task {t})")).unwrap_or_default();
                return Err(format!(
                    "{at} is running{task}; wait for it to return, or stop it with task_stop first"
                ));
            }
            // Steps kept word for word keep their ticks, and the last result
            // stays for the next subagent: rewriting a brief should not lose
            // what was done.
            let ticked = |text: &str| {
                before.is_some_and(|b| b.todos.iter().any(|t| t.done && t.text == text))
            };
            let todos: Vec<Todo> = args
                .todos
                .iter()
                .map(|text| nth_protocol::one_line(text))
                .filter(|text| !text.is_empty())
                .map(|text| Todo {
                    done: ticked(&text),
                    text,
                })
                .collect();
            let count = todos.len();
            let kept = todos.iter().filter(|t| t.done).count();
            let item = Item {
                result: before.and_then(|b| b.result.clone()),
                ..Item::pending(at.clone(), &args.title, &args.instruction, todos)
            };
            let _ = plan::ensure_dir(&super::root(ctx)).await;
            store
                .write(&item)
                .await
                .map_err(|e| format!("writing {at} failed: {e}"))?;
            let mut steps = match count {
                1 => "1 step".to_string(),
                n => format!("{n} steps"),
            };
            if kept > 0 {
                steps.push_str(&format!(", {kept} still ticked"));
            }
            let mut said = match before {
                Some(_) => format!("Wrote {at} again: pending, {steps}."),
                None => format!("Wrote {at}: pending, {steps}."),
            };
            // Closed until now, with a later cycle waiting on it: say that
            // it holds the later ones up again.
            let reopened = cycle.is_some_and(|cycle| !cycle.items.is_empty() && !cycle.is_open())
                && plan.cycles.iter().any(|c| c.number > args.cycle);
            if reopened {
                said.push_str(&format!(
                    " This opens cycle {} again: later cycles wait for it.",
                    at.cycle
                ));
            }
            Ok(said)
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work::{Store, tests::ctx, work_dir};

    fn args(cycle: u32, name: Option<&str>, item: &str) -> serde_json::Value {
        json!({
            "cycle": cycle,
            "cycle_name": name,
            "item": item,
            "title": "Store round-trip",
            "instruction": "Read and write item files.",
            "todos": ["parse", " ", "write"],
        })
    }

    #[tokio::test]
    async fn it_writes_the_item_file_and_names_the_cycle_once() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WorkItem::new(crate::work::tests::work());
        let ctx = ctx(dir.path());

        assert_eq!(
            tool.call(args(1, None, "store"), &ctx).await.unwrap_err(),
            "cycle 1 does not exist yet; give cycle_name to start it"
        );
        assert_eq!(
            tool.call(args(1, Some("foundation"), "store"), &ctx)
                .await
                .unwrap(),
            "Wrote 01-foundation/store: pending, 2 steps."
        );
        let file = std::fs::read_to_string(dir.path().join(".nth/work/s1/01-foundation/store.md"))
            .unwrap();
        assert_eq!(
            file,
            "---\nstatus: pending\n---\n\n# Store round-trip\n\nRead and write item files.\n\n## Todo\n\n- [ ] parse\n- [ ] write\n"
        );
        assert!(dir.path().join(".nth/.gitignore").exists());
        assert_eq!(
            tool.call(args(1, Some("other"), "notice"), &ctx)
                .await
                .unwrap_err(),
            "cycle 1 is 01-foundation; leave cycle_name out"
        );
        assert_eq!(
            tool.call(args(1, None, "store"), &ctx).await.unwrap(),
            "Wrote 01-foundation/store again: pending, 2 steps."
        );
        assert!(
            tool.call(args(1, None, "Bad"), &ctx)
                .await
                .unwrap_err()
                .contains("is not a slug")
        );
        let mut split = args(1, None, "x");
        split["instruction"] = json!("a\n## Todo\nb");
        assert!(
            tool.call(split, &ctx)
                .await
                .unwrap_err()
                .contains("## Todo")
        );
    }

    #[tokio::test]
    async fn a_running_item_is_not_rewritten_and_a_closed_cycle_opens_again() {
        let dir = tempfile::tempdir().unwrap();
        let work = crate::work::tests::work();
        let tool = WorkItem::new(work.clone());
        let ctx = ctx(dir.path());
        tool.call(args(1, Some("a"), "x"), &ctx).await.unwrap();
        tool.call(args(2, Some("b"), "y"), &ctx).await.unwrap();
        let store = Store::new(work_dir(dir.path(), "s1"));
        let at = ItemRef {
            cycle: "01-a".into(),
            item: "x".into(),
        };
        let mut item = store.read(&at).await.unwrap().unwrap();
        item.status = Status::Running;
        item.task = Some(4);
        store.write(&item).await.unwrap();
        work.files()
            .0
            .lock()
            .await
            .running
            .insert(at.path(store.dir()), 4);

        assert_eq!(
            tool.call(args(1, None, "x"), &ctx).await.unwrap_err(),
            "01-a/x is running (task 4); wait for it to return, or stop it with task_stop first"
        );

        item.status = Status::Done;
        item.todos[0].done = true;
        item.result = Some("parsed".into());
        store.write(&item).await.unwrap();
        let mut renamed = args(1, None, "x");
        renamed["todos"] = json!(["parse", "write it all"]);
        assert_eq!(
            tool.call(renamed, &ctx).await.unwrap(),
            "Wrote 01-a/x again: pending, 2 steps, 1 still ticked. This opens cycle 01-a again: later cycles wait for it."
        );
        let rewritten = store.read(&at).await.unwrap().unwrap();
        assert!(rewritten.todos[0].done && !rewritten.todos[1].done);
        assert_eq!(
            rewritten.result.as_deref(),
            Some("parsed"),
            "for the next subagent"
        );
        assert_eq!(
            tool.call(args(1, None, "z"), &ctx).await.unwrap(),
            "Wrote 01-a/z: pending, 2 steps.",
            "the cycle is open already"
        );
    }
}
