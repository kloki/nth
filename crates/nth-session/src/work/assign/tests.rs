use std::{path::Path, sync::Arc, time::Duration};

use nth_context::{Context, Paths};
use nth_protocol::{
    Inbox, Llm, NoticeSummary, StreamEvent, Tool, ToolContext, WorkOutcome, Writable, split_notices,
};
use serde_json::json;
use tokio::sync::mpsc;

use super::*;
use crate::{
    SubagentEvent, Subagents,
    agent_loop::tests::{Scripted, call},
    subagent::tests::says,
    work::{
        Item, Limits, Store,
        item::WorkItem,
        tests::{LIMITS, work_with},
        work_dir,
    },
};

fn ctx(dir: &Path, inbox: Inbox) -> ToolContext {
    ToolContext {
        session: "s1".into(),
        context: Arc::new(Context::discover(Path::new("/nowhere"), &Paths::default())),
        llm: Llm {
            model: "glm-5.3".into(),
            ..Llm::default()
        },
        inbox,
        ..ToolContext::new(dir.to_path_buf())
    }
}

/// Writes item `item` of cycle `cycle`, named after its letter.
async fn write(work: &Work, ctx: &ToolContext, cycle: u32, item: &str) {
    let args = json!({
        "cycle": cycle,
        "cycle_name": format!("c{cycle}"),
        "item": item,
        "title": "Store",
        "instruction": "Read and write item files.",
        "todos": ["parse", "write"],
    });
    WorkItem::new(work.clone()).call(args, ctx).await.unwrap();
}

fn assign(cycle: u32, item: &str) -> serde_json::Value {
    json!({ "cycle": cycle, "item": item, "subagent_type": "general" })
}

async fn read(dir: &Path, cycle: &str, item: &str) -> Item {
    let at = ItemRef {
        cycle: cycle.into(),
        item: item.into(),
    };
    Store::new(work_dir(dir, "s1"))
        .read(&at)
        .await
        .unwrap()
        .unwrap()
}

/// A child that ticks the first step, then answers.
fn ticks_then_answers() -> Vec<Vec<StreamEvent>> {
    vec![
        vec![StreamEvent::ToolCall(call(
            "1",
            "work_todo",
            r#"{"todos":[{"text":"parse","done":true},{"text":"write","done":false}]}"#,
        ))],
        says("Parsed; writing is left."),
    ]
}

async fn until(rx: &mut mpsc::Receiver<SubagentEvent>, pick: fn(&SubagentEvent) -> bool) {
    while let Some(event) = rx.recv().await {
        if pick(&event) {
            return;
        }
    }
    panic!("the subagent ended first");
}

fn ended(event: &SubagentEvent) -> bool {
    matches!(event, SubagentEvent::TurnEnded { .. })
}

#[tokio::test]
async fn headless_it_waits_while_the_child_ticks_its_todo() {
    let dir = tempfile::tempdir().unwrap();
    let work = work_with(
        ticks_then_answers(),
        Vec::new(),
        Subagents::default(),
        LIMITS,
    );
    let ctx = ctx(dir.path(), Inbox::default());
    write(&work, &ctx, 1, "store").await;

    let result = WorkAssign::new(work)
        .call(assign(1, "store"), &ctx)
        .await
        .unwrap();

    assert_eq!(
        result,
        "<work_item cycle=\"01-c1\" item=\"store\" task=\"1\" agent=\"general\" state=\"returned\" outcome=\"completed\" done=\"1\" total=\"2\">\n\
         <work_result>\nParsed; writing is left.\n</work_result>\n</work_item>"
    );
    let item = read(dir.path(), "01-c1", "store").await;
    assert_eq!(item.status, Status::Returned);
    assert_eq!(item.agent.as_deref(), Some("general"));
    assert_eq!(item.task, Some(1));
    assert!(item.assigned.is_some());
    assert_eq!(item.result.as_deref(), Some("Parsed; writing is left."));
    assert!(item.todos[0].done && !item.todos[1].done);
}

#[tokio::test]
async fn with_an_inbox_it_returns_at_once_and_every_change_is_a_notice() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(64);
    let work = work_with(ticks_then_answers(), Vec::new(), Subagents::new(tx), LIMITS);
    let inbox = Inbox::new();
    let ctx = ctx(dir.path(), inbox.clone());
    write(&work, &ctx, 1, "store").await;

    let result = WorkAssign::new(work.clone())
        .call(assign(1, "store"), &ctx)
        .await
        .unwrap();

    assert!(
        result
            .starts_with("Assigned 01-c1/store to subagent 1 (@general). Its answer and any stall"),
        "{result}"
    );
    assert_eq!(
        read(dir.path(), "01-c1", "store").await.status,
        Status::Running
    );
    until(&mut rx, ended).await;
    let text = inbox.take_notices().unwrap();
    let (notices, _) = split_notices(&text);
    let states: Vec<_> = notices
        .iter()
        .map(|n| match n {
            NoticeSummary::Work {
                state,
                outcome,
                progress,
                ..
            } => (state.as_str(), outcome.as_deref(), *progress),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        states,
        [
            ("todo", None, Some((1, 2))),
            ("returned", Some("completed"), Some((1, 2))),
        ]
    );
    assert_eq!(
        read(dir.path(), "01-c1", "store").await.status,
        Status::Returned
    );
    assert!(work.files().0.lock().await.running.is_empty());
}

#[tokio::test]
async fn cycles_run_one_after_another_and_closed_items_stay_closed() {
    let dir = tempfile::tempdir().unwrap();
    let work = work_with(
        vec![says("one"), says("two")],
        Vec::new(),
        Subagents::default(),
        LIMITS,
    );
    let ctx = ctx(dir.path(), Inbox::default());
    write(&work, &ctx, 1, "a").await;
    write(&work, &ctx, 2, "b").await;
    let tool = WorkAssign::new(work.clone());

    assert_eq!(
        tool.call(assign(2, "b"), &ctx).await.unwrap_err(),
        "cycle 01-c1 still has open items: a (pending). Cycles run one after another: close them first"
    );
    tool.call(assign(1, "a"), &ctx).await.unwrap();
    assert!(
        tool.call(assign(2, "b"), &ctx)
            .await
            .unwrap_err()
            .contains("a (returned)"),
        "a returned item still holds its cycle open"
    );
    let close = json!({ "cycle": 1, "item": "a", "outcome": "done" });
    super::super::close::WorkClose::new(work.clone())
        .call(close, &ctx)
        .await
        .unwrap();
    assert_eq!(
        tool.call(assign(1, "a"), &ctx).await.unwrap_err(),
        "01-c1/a is closed as done; write it again with work_item if there is more to do"
    );
    tool.call(assign(2, "b"), &ctx).await.unwrap();
    assert_eq!(
        tool.call(assign(2, "c"), &ctx).await.unwrap_err(),
        "there is no item 02-c2/c; write it with work_item first"
    );
    let unknown = json!({ "cycle": 2, "item": "b", "subagent_type": "nobody" });
    assert!(
        tool.call(unknown, &ctx)
            .await
            .unwrap_err()
            .starts_with("Agent \"nobody\" not found")
    );
}

#[tokio::test]
async fn task_id_continues_only_the_subagent_that_last_worked_on_the_item() {
    let dir = tempfile::tempdir().unwrap();
    let work = work_with(
        vec![says("first try"), says("second try")],
        Vec::new(),
        Subagents::default(),
        LIMITS,
    );
    let ctx = ctx(dir.path(), Inbox::default());
    write(&work, &ctx, 1, "a").await;
    write(&work, &ctx, 1, "b").await;
    let tool = WorkAssign::new(work);
    tool.call(assign(1, "a"), &ctx).await.unwrap();

    let again = |item: &str, id: u32| json!({ "cycle": 1, "item": item, "subagent_type": "general", "task_id": id });
    assert_eq!(
        tool.call(again("b", 1), &ctx).await.unwrap_err(),
        "subagent 1 did not work on 01-c1/b in this run of nth; leave task_id out to start a fresh one"
    );
    let result = tool.call(again("a", 1), &ctx).await.unwrap();
    assert!(result.contains("second try"), "{result}");
    assert_eq!(read(dir.path(), "01-c1", "a").await.task, Some(1));
}

#[tokio::test]
async fn a_quiet_subagent_is_reported_stalled_once_and_stopping_it_returns_the_item() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(64);
    let subagents = Subagents::new(tx);
    let limits = Limits {
        stall: Some(Duration::from_millis(20)),
        ..LIMITS
    };
    let work = Work::new(
        Arc::new(Scripted::stalled()),
        Vec::new(),
        subagents.clone(),
        limits,
    );
    let inbox = Inbox::new();
    let ctx = ctx(dir.path(), inbox.clone());
    write(&work, &ctx, 1, "a").await;

    WorkAssign::new(work)
        .call(assign(1, "a"), &ctx)
        .await
        .unwrap();
    until(&mut rx, |e| matches!(e, SubagentEvent::Stalled { .. })).await;
    // Well past another quiet period: nothing new was said, so no second
    // report.
    tokio::time::sleep(Duration::from_millis(80)).await;
    subagents.cancel(1);
    let mut stalls = 0;
    while let Some(event) = rx.recv().await {
        stalls += usize::from(matches!(event, SubagentEvent::Stalled { .. }));
        if ended(&event) {
            break;
        }
    }
    assert_eq!(stalls, 0, "one stall per quiet period");

    let text = inbox.take_notices().unwrap();
    let (notices, _) = split_notices(&text);
    let states: Vec<_> = notices
        .iter()
        .map(|n| match n {
            NoticeSummary::Work { state, outcome, .. } => (state.clone(), outcome.clone()),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        states,
        [
            ("stalled".to_string(), None),
            ("returned".to_string(), Some("interrupted".to_string())),
        ]
    );
    let item = read(dir.path(), "01-c1", "a").await;
    assert_eq!(item.status, Status::Returned);
    assert_eq!(item.outcome, Some(WorkOutcome::Interrupted));
    assert_eq!(item.result.as_deref(), Some(super::super::STOPPED));
}

#[tokio::test]
async fn a_left_session_still_returns_its_items_but_tells_no_model() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(64);
    let subagents = Subagents::new(tx);
    let work = Work::new(
        Arc::new(Scripted::stalled()),
        Vec::new(),
        subagents.clone(),
        LIMITS,
    );
    let inbox = Inbox::new();
    let ctx = ctx(dir.path(), inbox.clone());
    write(&work, &ctx, 1, "a").await;
    let tool = WorkAssign::new(work.clone());
    tool.call(assign(1, "a"), &ctx).await.unwrap();
    until(&mut rx, |e| matches!(e, SubagentEvent::Prompted { .. })).await;

    subagents.forget_all();
    until(&mut rx, ended).await;

    assert!(!inbox.has_notices(), "the session that asked was left");
    assert_eq!(
        read(dir.path(), "01-c1", "a").await.status,
        Status::Returned
    );
    assert!(
        work.files().0.lock().await.running.is_empty(),
        "so coming back to it can assign it again"
    );
}

#[tokio::test]
async fn dropping_the_handles_ends_a_work_subagent() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(64);
    let work = work_with(vec![says("done")], Vec::new(), Subagents::new(tx), LIMITS);
    let ctx = ctx(dir.path(), Inbox::new());
    write(&work, &ctx, 1, "a").await;
    let tool = WorkAssign::new(work.clone());
    tool.call(assign(1, "a"), &ctx).await.unwrap();
    until(&mut rx, ended).await;

    // The idle subagent holds its work_todo; if that held the registry,
    // the registry would never drop and the actor never end.
    drop((tool, work));
    let rest = tokio::time::timeout(Duration::from_secs(5), async {
        while rx.recv().await.is_some() {}
    })
    .await;
    assert!(rest.is_ok(), "every sender went with the handles");
}

/// A tool that is only a name.
struct Named(&'static str);

impl Tool for Named {
    fn spec(&self) -> nth_protocol::ToolSpec {
        nth_protocol::ToolSpec {
            name: self.0,
            description: String::new(),
            parameters: json!({}),
        }
    }

    fn call<'a>(
        &'a self,
        _: serde_json::Value,
        _: &'a ToolContext,
    ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
        Box::pin(async { Ok(String::new()) })
    }
}

#[tokio::test]
async fn a_planning_orchestrators_subagent_gets_no_writers() {
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(64);
    let subagents = Subagents::new(tx);
    let tools: Vec<Arc<dyn Tool>> = ["read", "write", "edit"]
        .into_iter()
        .map(|name| Arc::new(Named(name)) as Arc<dyn Tool>)
        .collect();
    let provider = Arc::new(Scripted::new(vec![says("planned")]));
    let work = Work::new(provider, tools, subagents.clone(), LIMITS);
    let ctx = ToolContext {
        writable: Writable::Only(dir.path().join(".nth/plans/x.md")),
        ..ctx(dir.path(), Inbox::new())
    };
    write(&work, &ctx, 1, "a").await;

    WorkAssign::new(work)
        .call(assign(1, "a"), &ctx)
        .await
        .unwrap();

    assert!(!subagents.writes(1));
    let prompt = loop {
        match rx.recv().await {
            Some(SubagentEvent::Prompted { text, .. }) => break text,
            Some(_) => {}
            None => panic!("never prompted"),
        }
    };
    assert!(prompt.contains("may not change the project"), "{prompt}");
    assert!(prompt.contains("<work_item>\n# Store\n"), "{prompt}");
    assert!(prompt.contains("- [ ] parse\n- [ ] write"), "{prompt}");
}

/// Polls until the item's file says `status`, or gives up.
async fn until_status(dir: &Path, item: &str, status: Status) -> Item {
    for _ in 0..200 {
        let read = read(dir, "01-c1", item).await;
        if read.status == status {
            return read;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{item} never became {status}");
}

#[tokio::test]
async fn a_headless_call_dropped_while_it_waits_still_returns_the_item() {
    let dir = tempfile::tempdir().unwrap();
    // The receiver stays, so the actor's events always have room.
    let (tx, _rx) = mpsc::channel(64);
    let work = Work::new(
        Arc::new(Scripted::stalled()),
        Vec::new(),
        Subagents::new(tx),
        LIMITS,
    );
    // No inbox, so the call waits as `nth run`'s does.
    let ctx = ctx(dir.path(), Inbox::default());
    write(&work, &ctx, 1, "a").await;
    let tool = WorkAssign::new(work.clone());

    // Dropped once the item is running, as a cancelled turn drops the
    // call it waits in. Dropped before, the item stays pending.
    tokio::select! {
        result = tool.call(assign(1, "a"), &ctx) => panic!("the model never answers: {result:?}"),
        _ = until_status(dir.path(), "a", Status::Running) => {}
    }

    let item = until_status(dir.path(), "a", Status::Returned).await;
    assert_eq!(item.outcome, Some(WorkOutcome::Interrupted));
    assert!(work.files().0.lock().await.running.is_empty());
}

#[tokio::test]
async fn a_front_end_gone_still_returns_the_items_queued_for_it() {
    let dir = tempfile::tempdir().unwrap();
    // Full, so the actor waits to announce itself until the receiver goes.
    let (tx, rx) = mpsc::channel(1);
    tx.try_send(SubagentEvent::Prompted {
        id: 99,
        text: "filler".into(),
    })
    .unwrap();
    let work = Work::new(
        Arc::new(Scripted::stalled()),
        Vec::new(),
        Subagents::new(tx),
        LIMITS,
    );
    let ctx = ctx(dir.path(), Inbox::new());
    write(&work, &ctx, 1, "a").await;
    WorkAssign::new(work.clone())
        .call(assign(1, "a"), &ctx)
        .await
        .unwrap();

    drop(rx);

    let item = until_status(dir.path(), "a", Status::Returned).await;
    assert_eq!(item.outcome, Some(WorkOutcome::Interrupted));
    assert!(work.files().0.lock().await.running.is_empty());
}

#[tokio::test]
async fn the_plan_stays_in_the_main_checkout_when_the_session_enters_a_worktree() {
    let dir = tempfile::tempdir().unwrap();
    let work = work_with(vec![says("done")], Vec::new(), Subagents::default(), LIMITS);
    let main = ctx(dir.path(), Inbox::default());
    write(&work, &main, 1, "a").await;
    let workdir = nth_protocol::Workdir::default();
    let tree = dir.path().join(".nth/worktrees/feat");
    workdir.enter(dir.path(), tree.clone());
    let moved = ToolContext {
        cwd: tree,
        workdir,
        ..ctx(dir.path(), Inbox::default())
    };

    let result = WorkAssign::new(work).call(assign(1, "a"), &moved).await;

    assert!(result.unwrap().contains("state=\"returned\""));
    assert_eq!(
        read(dir.path(), "01-c1", "a").await.status,
        Status::Returned
    );
}

#[tokio::test]
async fn a_timeout_of_zero_is_no_limit() {
    let dir = tempfile::tempdir().unwrap();
    let work = work_with(vec![says("done")], Vec::new(), Subagents::default(), LIMITS);
    let ctx = ctx(dir.path(), Inbox::default());
    write(&work, &ctx, 1, "a").await;
    let args = json!({ "cycle": 1, "item": "a", "subagent_type": "general", "timeout_secs": 0 });

    let result = WorkAssign::new(work).call(args, &ctx).await.unwrap();

    assert!(result.contains("outcome=\"completed\""), "{result}");
}
