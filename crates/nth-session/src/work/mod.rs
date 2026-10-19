//! Work: the orchestration of a long piece of work. The model writes a plan
//! of serial cycles, each of items worked on in parallel, one subagent an
//! item, and hears every change to an item as a `<work_item>` notice.
//! Nothing here moves on by itself: the tools guard the order (a cycle's
//! items wait for every earlier cycle to be closed, a running item is not
//! rewritten) and report; the model decides.

mod assign;
mod close;
mod item;
pub mod outline;
mod status;
pub mod store;
mod todo;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

pub use assign::Assigned;
use assign::WorkAssign;
use close::WorkClose;
use item::WorkItem;
use nth_protocol::{
    Inbox, Provider, TaskId, Tool, ToolContext, WorkNotice, WorkOutcome, WorkState,
};
use status::WorkStatus;
pub use store::{Cycle, Item, ItemRef, Plan, Status, Store, cycle_name, work_dir};

use crate::{
    Error, SubagentId, Subagents,
    subagent::{self, Spawner},
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
    /// an entry here was left so by a run of nth that ended.
    running: BTreeMap<PathBuf, SubagentId>,
    /// The last one on each item, running or not: the only one `task_id`
    /// may continue it with, since an id in a file may be another run's.
    worked: BTreeMap<PathBuf, SubagentId>,
}

/// The lock every change to an item file is made under, which owns what
/// it guards: a file that says `running` and `Live` never disagree while
/// a tool looks.
#[derive(Debug, Default)]
pub(crate) struct Files(tokio::sync::Mutex<Live>);

/// What an assigned item's subagent may spend.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Its steps, and how long its turn may run before the item returns
    /// as timed out.
    pub subagent: subagent::Limits,
    /// How long it may say nothing before the orchestrator hears it
    /// stalled; `None` never.
    pub stall: Option<Duration>,
}

/// What the work tools share; cheap to clone. Only the orchestrator's
/// tools hold it: what a subagent's tool holds must not hold `Subagents`,
/// which holds the subagent.
#[derive(Clone)]
pub struct Work(Arc<Shared>);

struct Shared {
    spawner: Spawner,
    /// How long an assigned subagent may say nothing; see [`Limits`].
    stall: Option<Duration>,
    files: Arc<Files>,
}

impl Work {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        subagents: Subagents,
        limits: Limits,
    ) -> Self {
        Self(Arc::new(Shared {
            spawner: Spawner::new(provider, tools, subagents, limits.subagent),
            stall: limits.stall,
            files: Arc::default(),
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
        let live = self.files().0.lock().await;
        let plan = reconciled(store, &live).await?;
        Ok((live, plan))
    }

    /// The store of the session the call runs for.
    fn store(ctx: &ToolContext) -> Result<Store, String> {
        if ctx.session.is_empty() {
            return Err("this call runs outside a session, which has no work plan".into());
        }
        let root = root(ctx.workdir.origin().as_deref(), &ctx.cwd);
        Ok(Store::new(work_dir(&root, &ctx.session)))
    }
}

/// Where a session in `cwd` that left `origin` for a worktree keeps its
/// work: the main checkout, even after a worktree tool moved the session,
/// so the plan stays one plan and the subagents still on it keep writing
/// where the model reads.
pub(crate) fn root(origin: Option<&Path>, cwd: &Path) -> PathBuf {
    origin.unwrap_or(cwd).to_path_buf()
}

/// The plan, with every item a gone run of nth left running settled as
/// interrupted: no subagent of this process is on it. Subagent ids start
/// again with every run, so the id in the file says nothing here.
async fn reconciled(store: &Store, live: &Live) -> Result<Plan, String> {
    let mut plan = store
        .load()
        .await
        .map_err(|e| format!("reading the work plan failed: {e}"))?
        .unwrap_or_default();
    for item in plan.cycles.iter_mut().flat_map(|cycle| &mut cycle.items) {
        if item.status != Status::Running || live.running.contains_key(&item.at.path(store.dir())) {
            continue;
        }
        item.status = Status::Returned;
        item.outcome = Some(nth_protocol::WorkOutcome::Interrupted);
        item.result = Some(GONE.into());
        store
            .write(item)
            .await
            .map_err(|e| format!("writing {} failed: {e}", item.at))?;
    }
    Ok(plan)
}

/// How a turn ended, as a work item keeps it.
fn outcome(turn: &crate::subagent::Turn) -> WorkOutcome {
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
/// item is no longer running in this process. Returns the notice telling
/// the orchestrator. Takes the lock itself, so the caller must not hold it.
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
    if live.running.get(&path) == Some(task) {
        live.running.remove(&path);
    }
    let mut progress = (0, 0);
    // A missing or rewritten file is the orchestrator's now; the notice
    // still tells it how the turn ended.
    if let Ok(Some(mut item)) = store.read(at).await {
        progress = item.progress();
        if item.status == Status::Running && item.task == Some(*task) {
            item.status = Status::Returned;
            item.result = Some(match outcome.text().trim() {
                "" => STOPPED.to_string(),
                text => text.to_string(),
            });
            item.outcome = Some(outcome.clone());
            // Nothing more to do on failure: the notice carries what the
            // file would have.
            let _ = store.write(&item).await;
        }
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
    task: TaskId,
    agent: String,
    /// The orchestrator's.
    inbox: Inbox,
    /// Cancelled once the subagent is forgotten with the session that
    /// assigned it: what it says after goes to no model.
    closed: tokio_util::sync::CancellationToken,
}

/// A name for a cycle or an item: lowercase letters and digits, words
/// joined by single hyphens, at most 48 characters.
pub fn slug(text: &str) -> Result<(), String> {
    let fine = !text.is_empty()
        && text.len() <= 48
        && text.split('-').all(|word| {
            !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        });
    match fine {
        true => Ok(()),
        false => Err(format!(
            "{text:?} is not a slug: use lowercase letters and digits, words joined by single hyphens, at most 48 characters"
        )),
    }
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
        limits: Limits,
    ) -> Work {
        Work::new(Arc::new(Scripted::new(replies)), tools, subagents, limits)
    }

    pub(crate) const LIMITS: Limits = Limits {
        subagent: subagent::Limits {
            max_steps: 10,
            timeout: None,
        },
        stall: None,
    };

    /// Work tools with no subagent to run.
    pub(crate) fn work() -> Work {
        work_with(Vec::new(), Vec::new(), Subagents::default(), LIMITS)
    }

    /// A call in session `s1`, in `dir`.
    pub(crate) fn ctx(dir: &std::path::Path) -> ToolContext {
        ToolContext {
            session: "s1".into(),
            ..ToolContext::new(dir.to_path_buf())
        }
    }

    #[test]
    fn slugs_are_lowercase_words() {
        for fine in ["store", "a-b-2", "x", &"a".repeat(48)] {
            assert_eq!(slug(fine), Ok(()), "{fine}");
        }
        for bad in ["", "Foo", "a--b", "-a", "a-", "a b", "ä", &"a".repeat(49)] {
            assert!(slug(bad).is_err(), "{bad}");
        }
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

        let plan = reconciled(&store, &live).await.unwrap();
        assert_eq!(
            plan.item(&at).unwrap().status,
            Status::Running,
            "still ours"
        );

        let plan = reconciled(&store, &Live::default()).await.unwrap();
        let item = plan.item(&at).unwrap();
        assert_eq!(item.status, Status::Returned);
        assert_eq!(item.outcome, Some(nth_protocol::WorkOutcome::Interrupted));
        assert_eq!(
            store.read(&at).await.unwrap().unwrap().status,
            Status::Returned,
            "and the file says so"
        );
        assert!(Work::store(&ToolContext::new(dir.path().into())).is_err());
    }
}
