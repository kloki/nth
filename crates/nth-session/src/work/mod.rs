//! Work: the orchestration of a long piece of work. The model writes a plan
//! of serial cycles, each of items worked on in parallel, one subagent an
//! item, and hears every change to an item as a `<work_item>` notice.
//! Nothing here moves on by itself: the tools guard the order (a cycle's
//! items wait for every earlier cycle to be closed, a running item is not
//! rewritten) and report; the model decides.

mod assign;
mod close;
mod item;
mod notice;
mod status;
mod todo;

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

pub use assign::Assigned;
use assign::WorkAssign;
use close::WorkClose;
use item::WorkItem;
pub use notice::{WorkNotice, WorkState};
use nth_protocol::{Provider, Tool, ToolContext};
use nth_work::{Cycle, Item, ItemRef, Plan, Status, Store, WorkOutcome, work_dir};
use status::WorkStatus;

use crate::{
    Error, Inbox, SubagentId, Subagents,
    subagent::{Limits, Spawner},
};

/// The tools that write and read the plan, which no subagent gets: the one
/// on an item sees that item only.
pub const ORCHESTRATOR_TOOLS: [&str; 4] = ["work_item", "work_assign", "work_close", "work_status"];

/// Told to the model for an item left running by a run of nth that ended.
const GONE: &str = "nth stopped while this item ran, and its subagent with it. Assign it again, without task_id: its ticked steps are kept.";

/// Which item files the subagents of this process work on, by path.
#[derive(Debug, Default)]
pub(crate) struct Live {
    /// The one on each running item. A file that says `running` without
    /// an entry here or in `unreturned` was left so by a run of nth that
    /// ended.
    running: BTreeMap<PathBuf, SubagentId>,
    /// How the turn ended on each item whose file still says `running`
    /// because writing its return failed: the next work tool call writes
    /// it again rather than taking the file for a gone run's. Process
    /// memory is the right scope, since a run that ends takes its
    /// subagents with it.
    unreturned: BTreeMap<PathBuf, (SubagentId, WorkOutcome)>,
    /// The last one on each item, running or not: the only one `task_id`
    /// may continue it with, since an id in a file may be another run's.
    worked: BTreeMap<PathBuf, SubagentId>,
}

/// The lock every change to an item file is made under, which owns what
/// it guards: a file that says `running` and `Live` never disagree while
/// a tool looks.
#[derive(Debug, Default)]
pub(crate) struct Files(tokio::sync::Mutex<Live>);

/// What the work tools share; cheap to clone. Only the orchestrator's
/// tools hold it: what a subagent's tool holds must not hold `Subagents`,
/// which holds the subagent.
#[derive(Clone)]
pub struct Work(Arc<Shared>);

struct Shared {
    spawner: Spawner,
    files: Arc<Files>,
    /// Where the items' changes wait for the model: the front-end's, or
    /// none headless, when `work_assign` waits for the return itself.
    inbox: Inbox,
}

impl Work {
    /// `limits` are what an assigned item's subagent may spend: the item
    /// returns as timed out past its time, and the orchestrator hears it
    /// stalled once it has said nothing for long enough. `inbox` must be
    /// the one the front-end empties, or the notices go to no model.
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        subagents: Subagents,
        inbox: Inbox,
        limits: Limits,
    ) -> Self {
        Self(Arc::new(Shared {
            spawner: Spawner::new(provider, tools, subagents, limits),
            files: Arc::default(),
            inbox,
        }))
    }

    /// The orchestrator's tools.
    pub fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(WorkItem::new(self.clone())),
            Box::new(WorkAssign::new(self.clone())),
            Box::new(WorkClose::new(self.clone())),
            Box::new(WorkStatus::new(self.clone())),
        ]
    }

    fn files(&self) -> &Files {
        &self.0.files
    }

    /// The lock every change to an item file is made under, and the plan
    /// in `store` read under it.
    async fn locked_plan(
        &self,
        store: &Store,
    ) -> Result<(tokio::sync::MutexGuard<'_, Live>, Plan), String> {
        let mut live = self.files().0.lock().await;
        let plan = reconciled(store, &mut live).await?;
        Ok((live, plan))
    }

    /// The store of the session the call runs for.
    fn store(ctx: &ToolContext) -> Result<Store, String> {
        if ctx.session.is_empty() {
            return Err("this call runs outside a session, which has no work plan".into());
        }
        let root = root(ctx);
        Ok(Store::new(work_dir(&root, &ctx.session)))
    }
}

/// Where this session keeps its work, even after changing checkout.
pub(crate) fn root(ctx: &ToolContext) -> PathBuf {
    ctx.state_root.clone().unwrap_or_else(|| {
        let origin = ctx.workdir.origin();
        let cwd = origin.as_deref().unwrap_or(&ctx.cwd);
        nth_directories::main_project_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
    })
}

/// The plan, with every item no subagent of this process is on settled:
/// returned as its turn ended when writing that failed before, else as
/// interrupted, left running by a gone run of nth. Subagent ids start
/// again with every run, so the id in the file says nothing here.
async fn reconciled(store: &Store, live: &mut Live) -> Result<Plan, String> {
    let mut plan = store
        .load()
        .await
        .map_err(|e| format!("reading the work plan failed: {e}"))?
        .unwrap_or_default();
    for item in plan.cycles.iter_mut().flat_map(|cycle| &mut cycle.items) {
        let path = item.at.path(store.dir());
        // Taken whatever the file says: one no longer running was written
        // since and is the orchestrator's, so its return is moot.
        let unreturned = live.unreturned.remove(&path);
        if item.status != Status::Running || live.running.contains_key(&path) {
            continue;
        }
        match &unreturned {
            Some((task, outcome)) if item.task == Some(*task) => returns(item, outcome.clone()),
            _ => {
                item.status = Status::Returned;
                item.outcome = Some(WorkOutcome::Interrupted);
                item.result = Some(GONE.into());
            }
        }
        if let Err(e) = store.write(item).await {
            if let Some(unreturned) = unreturned {
                live.unreturned.insert(path, unreturned);
            }
            return Err(format!("writing {} failed: {e}", item.at));
        }
    }
    Ok(plan)
}

/// `item` returns as its subagent's turn ended.
fn returns(item: &mut Item, outcome: WorkOutcome) {
    item.status = Status::Returned;
    item.result = Some(match outcome.text().trim() {
        "" => STOPPED.to_string(),
        text => text.to_string(),
    });
    item.outcome = Some(outcome);
}

/// How a turn ended, as a work item keeps it.
fn outcome(turn: &crate::subagent::Answer) -> WorkOutcome {
    match turn {
        Ok(answer) => WorkOutcome::Completed(answer.clone()),
        Err(Error::Interrupted) => WorkOutcome::Interrupted,
        Err(e @ Error::TimedOut(_)) => WorkOutcome::TimedOut(e.to_string()),
        Err(e) => WorkOutcome::Failed(e.to_string()),
    }
}

/// Told to the next subagent on an item whose last one was stopped.
const STOPPED: &str = "The last subagent on this item was stopped before it answered.";

/// Subagent `task`'s turn on the item at `at` ended with `outcome`: the
/// file returns with it, unless someone wrote the item since, and the
/// item is no longer running in this process. A write that fails, or a
/// read, leaves the outcome in `Live.unreturned` for the next work tool
/// call to write. Returns the notice telling the orchestrator. Takes the
/// lock itself, so the caller must not hold it.
async fn returned(assignment: &Assignment, outcome: WorkOutcome) -> WorkNotice {
    let Assignment {
        files,
        store,
        at,
        task,
        agent,
        ..
    } = assignment;
    let path = at.path(store.dir());
    let mut live = files.0.lock().await;
    let mut progress = (0, 0);
    // A missing or rewritten file is the orchestrator's now; the notice
    // still tells it how the turn ended.
    let written = match store.read(at).await {
        Ok(Some(mut item)) => {
            progress = item.progress();
            if item.status == Status::Running && item.task == Some(*task) {
                returns(&mut item, outcome.clone());
                store.write(&item).await.is_ok()
            } else {
                true
            }
        }
        Ok(None) => true,
        // Unknown whether it is still ours: `reconciled` writes it only
        // if the file still says running with this task.
        Err(_) => false,
    };
    // Kept for the next work tool call to write: a file still saying
    // running with no entry is taken for a gone run's, and the answer
    // would be overwritten.
    if !written {
        live.unreturned
            .insert(path.clone(), (*task, outcome.clone()));
    }
    if live.running.get(&path) == Some(task) {
        live.running.remove(&path);
    }
    WorkNotice {
        cycle: at.cycle.clone(),
        item: at.item.clone(),
        task: *task,
        agent: agent.clone(),
        state: WorkState::Returned {
            outcome,
            done: progress.0,
            total: progress.1,
        },
    }
}

/// Where a subagent's tool reaches its item and the model that assigned
/// it. Holds no `Work`, so no `Subagents`: a subagent holding its own
/// registry would never end.
#[derive(Clone)]
struct Assignment {
    files: Arc<Files>,
    store: Store,
    at: ItemRef,
    task: SubagentId,
    agent: String,
    /// The orchestrator's.
    inbox: Inbox,
    /// Cancelled once the subagent is forgotten with the session that
    /// assigned it: what it says after goes to no model.
    closed: tokio_util::sync::CancellationToken,
}

/// `01-foundation/store: running` style lists of what keeps a cycle open.
fn open_items(cycle: &Cycle) -> String {
    cycle
        .items
        .iter()
        .filter(|item| item.status.is_open())
        .map(|item| format!("{} ({})", item.at.item, item.status))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
pub(crate) mod tests {
    use nth_protocol::ToolContext;

    use super::*;
    use crate::agent_loop::tests::Scripted;

    /// Work tools whose subagents would answer `replies`, one turn each.
    pub(crate) fn work_with(
        replies: Vec<Vec<nth_protocol::StreamEvent>>,
        tools: Vec<Arc<dyn Tool>>,
        subagents: Subagents,
        inbox: Inbox,
        limits: Limits,
    ) -> Work {
        Work::new(
            Arc::new(Scripted::new(replies)),
            tools,
            subagents,
            inbox,
            limits,
        )
    }

    pub(crate) const LIMITS: Limits = Limits {
        max_steps: 10,
        timeout: None,
        stall: None,
    };

    /// Work tools with no subagent to run.
    pub(crate) fn work() -> Work {
        work_with(
            Vec::new(),
            Vec::new(),
            Subagents::default(),
            Inbox::default(),
            LIMITS,
        )
    }

    /// A call in session `s1`, in `dir`.
    pub(crate) fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            session: "s1".into(),
            ..ToolContext::new(dir.to_path_buf())
        }
    }

    /// Sets the Unix permission bits of `dir`.
    pub(crate) fn chmod(dir: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[tokio::test]
    async fn an_unreturned_outcome_is_written_only_to_its_own_running_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(work_dir(dir.path(), "s1"));
        let item = |name: &str, status, task| Item {
            status,
            task,
            ..Item::pending(
                ItemRef {
                    cycle: "01-a".into(),
                    item: name.into(),
                },
                "X",
                "",
                Vec::new(),
            )
        };
        let other = item("other", Status::Running, Some(4));
        let pending = item("pending", Status::Pending, None);
        store.write(&other).await.unwrap();
        store.write(&pending).await.unwrap();
        let before = store.read(&pending.at).await.unwrap();
        let answer = WorkOutcome::Completed("All parsed.".into());
        let mut live = Live::default();
        for at in [&other.at, &pending.at] {
            live.unreturned
                .insert(at.path(store.dir()), (3, answer.clone()));
        }

        let plan = reconciled(&store, &mut live).await.unwrap();

        let reassigned = plan.item(&other.at).unwrap();
        assert_eq!(reassigned.outcome, Some(WorkOutcome::Interrupted));
        assert_eq!(reassigned.result.as_deref(), Some(GONE));
        assert_eq!(
            store.read(&pending.at).await.unwrap(),
            before,
            "untouched, to its modified time"
        );
        assert!(live.unreturned.is_empty());
    }

    #[tokio::test]
    async fn a_returned_item_whose_write_failed_is_written_on_the_next_call() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(dir.path());
        let store = Work::store(&ctx).unwrap();
        let at = ItemRef {
            cycle: "01-a".into(),
            item: "x".into(),
        };
        let running = Item {
            status: Status::Running,
            task: Some(3),
            ..Item::pending(at.clone(), "X", "", Vec::new())
        };
        store.write(&running).await.unwrap();
        let work = work();
        let path = at.path(store.dir());
        work.files().0.lock().await.running.insert(path.clone(), 3);
        let assignment = Assignment {
            files: work.0.files.clone(),
            store: store.clone(),
            at: at.clone(),
            task: 3,
            agent: "general".into(),
            inbox: Inbox::default(),
            closed: Default::default(),
        };
        let cycle = path.parent().unwrap();
        let status = WorkStatus::new(work.clone());

        chmod(cycle, 0o500);
        let answer = WorkOutcome::Completed("All parsed.".into());
        returned(&assignment, answer.clone()).await;
        assert_eq!(
            store.read(&at).await.unwrap().unwrap().status,
            Status::Running,
            "the write failed"
        );
        let retried = status.call(serde_json::json!({}), &ctx).await;
        chmod(cycle, 0o700);
        assert!(retried.is_err(), "{retried:?}");
        assert!(
            work.files().0.lock().await.unreturned.contains_key(&path),
            "kept for the call after"
        );

        let result = status.call(serde_json::json!({}), &ctx).await;
        assert!(result.is_ok(), "{result:?}");
        let item = store.read(&at).await.unwrap().unwrap();
        assert_eq!(item.status, Status::Returned);
        assert_eq!(item.outcome, Some(answer));
        assert_eq!(item.result.as_deref(), Some("All parsed."));
        let live = work.files().0.lock().await;
        assert!(live.running.is_empty() && live.unreturned.is_empty());
    }

    #[tokio::test]
    async fn an_item_left_running_by_a_gone_run_returns_interrupted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(work_dir(dir.path(), "s1"));
        let at = ItemRef {
            cycle: "01-a".into(),
            item: "x".into(),
        };
        let running = Item {
            status: Status::Running,
            task: Some(3),
            ..Item::pending(at.clone(), "X", "", Vec::new())
        };
        store.write(&running).await.unwrap();
        let mut live = Live::default();
        live.running.insert(at.path(store.dir()), 3);

        let plan = reconciled(&store, &mut live).await.unwrap();
        assert_eq!(
            plan.item(&at).unwrap().status,
            Status::Running,
            "still ours"
        );

        let plan = reconciled(&store, &mut Live::default()).await.unwrap();
        let item = plan.item(&at).unwrap();
        assert_eq!(item.status, Status::Returned);
        assert_eq!(item.outcome, Some(WorkOutcome::Interrupted));
        assert_eq!(
            store.read(&at).await.unwrap().unwrap().status,
            Status::Returned,
            "and the file says so"
        );
        assert!(Work::store(&ToolContext::new(dir.path().into())).is_err());
    }
}
