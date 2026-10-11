//! Work: the orchestration of a long piece of work. The model writes a plan
//! of serial cycles, each of items worked on in parallel, one subagent an
//! item, and hears every change to an item as a `<work_item>` notice.
//! Nothing here moves on by itself: the tools guard the order (a cycle's
//! items wait for every earlier cycle to be closed, a running item is not
//! rewritten) and report; the model decides.

mod close;
mod item;
mod status;
pub mod store;

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use close::WorkClose;
use item::WorkItem;
use nth_protocol::{Tool, ToolContext};
use status::WorkStatus;
pub use store::{Cycle, Item, ItemRef, Plan, Status, Store, cycle_name, work_dir};

use crate::SubagentId;

/// The tools that write and read the plan, which no subagent gets: the one
/// on an item sees that item only.
pub const ORCHESTRATOR_TOOLS: [&str; 4] = ["work_item", "work_assign", "work_close", "work_status"];

/// Told to the model for an item left running by a run of nth that ended.
const GONE: &str = "nth stopped while this item ran, and its subagent with it. Assign it again, without task_id: its ticked steps are kept.";

/// Item files by path, and the subagent of this process on each running
/// one. Every change to an item file is made holding it, so a file that
/// says `running` and this map never disagree for long enough to matter.
type Live = BTreeMap<PathBuf, SubagentId>;

#[derive(Default)]
pub(crate) struct Files(tokio::sync::Mutex<Live>);

/// What the work tools share; cheap to clone.
#[derive(Clone, Default)]
pub struct Work {
    files: Arc<Files>,
}

impl Work {
    pub fn new() -> Self {
        Self::default()
    }

    /// The orchestrator's tools.
    pub fn tools(&self) -> Vec<Box<dyn Tool>> {
        vec![
            Box::new(WorkItem::new(self.clone())),
            Box::new(WorkClose::new(self.clone())),
            Box::new(WorkStatus::new(self.clone())),
        ]
    }

    /// The store of the session the call runs for.
    fn store(ctx: &ToolContext) -> Result<Store, String> {
        if ctx.session.is_empty() {
            return Err("this call runs outside a session, which has no work plan".into());
        }
        Ok(Store::new(work_dir(&ctx.cwd, &ctx.session)))
    }
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
        if item.status != Status::Running || live.contains_key(&item.at.path(store.dir())) {
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
        let mut live = Live::new();
        live.insert(at.path(store.dir()), 3);

        let plan = reconciled(&store, &live).await.unwrap();
        assert_eq!(
            plan.item(&at).unwrap().status,
            Status::Running,
            "still ours"
        );

        let plan = reconciled(&store, &Live::new()).await.unwrap();
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
