//! `work_assign`: puts a subagent on an item. With a front-end the call
//! returns at once and the item's changes come back as notices; headless
//! it waits for the subagent's answer, as the task tool does.

use std::{
    sync::{Arc, OnceLock},
    time::Duration,
};

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{
    Assignment, ItemRef, Status, Work, WorkNotice, WorkState, open_items, outcome, returned,
    todo::WorkTodo,
};
use crate::subagent::{Answer, Done, Job, PLANNING, TaskIdArg, find_agent};

const ASSIGNED: &str = include_str!("assigned.md");

/// What the actor tells about a job on a work item. The actor settles
/// it however the job ends, so the item never stays running for nobody.
pub struct Assigned {
    assignment: Assignment,
    /// The headless call waiting for the notice, as it has no inbox.
    reply: Option<oneshot::Sender<WorkNotice>>,
}

impl Assigned {
    /// The same, telling no model: the session that assigned it was left.
    pub(crate) fn unheard(mut self) -> Self {
        self.assignment.inbox = crate::Inbox::default();
        self
    }

    /// The subagent has said nothing for `quiet`.
    pub(crate) fn stalled(&self, quiet: Duration) {
        let a = &self.assignment;
        a.inbox.post_work(WorkNotice {
            cycle: a.at.cycle.clone(),
            item: a.at.item.clone(),
            task: a.task,
            agent: a.agent.clone(),
            state: WorkState::Stalled { quiet },
        });
    }

    /// The subagent's turn ended: the item returns and the model hears.
    pub(crate) async fn settle(self, turn: Answer) {
        let notice = returned(&self.assignment, outcome(&turn)).await;
        self.assignment.inbox.post_work(notice.clone());
        // A dropped receiver is a call that stopped waiting, which is fine.
        if let Some(reply) = self.reply {
            let _ = reply.send(notice);
        }
    }
}

pub(super) struct WorkAssign {
    work: Work,
}

impl WorkAssign {
    pub(super) fn new(work: Work) -> Self {
        Self { work }
    }
}

#[derive(Deserialize)]
struct Args {
    cycle: u32,
    item: String,
    subagent_type: String,
    task_id: Option<TaskIdArg>,
    timeout_ms: Option<u64>,
}

impl Tool for WorkAssign {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "work_assign",
            description: include_str!("assign.md").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "cycle": { "type": "integer", "description": "The item's cycle number." },
                    "item": { "type": "string", "description": "The item's slug." },
                    "subagent_type": { "type": "string", "description": "The type of specialized agent to put on it" },
                    "task_id": { "type": "integer", "description": "Only to continue the subagent that last worked on this item, with what it remembers; leave it out for a fresh one." },
                    "timeout_ms": { "type": "integer", "minimum": 0, "description": "Milliseconds this attempt may run before it returns as timed out, 0 for no limit; the configured limit by default." }
                },
                "required": ["cycle", "item", "subagent_type"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            nth_protocol::reject_old_timeout(&args)?;
            let args: Args = nth_protocol::parse_args(args)?;
            if ctx.llm.model.is_empty() {
                return Err("this session has no model to run a subagent on".into());
            }
            let store = Work::store(ctx)?;
            let shared = &self.work.0;
            let subagents = shared.spawner.subagents();
            let agent = find_agent(ctx, &args.subagent_type)?;
            let planning = ctx.writable.planning();
            let continued = args.task_id.as_ref().map(TaskIdArg::parse).transpose()?;
            // Held from reading the plan until the file says running and
            // `Live` knows it, so two calls cannot both take one item.
            let (mut live, plan) = self.work.locked_plan(&store).await?;
            let Some(cycle) = plan.cycle(args.cycle) else {
                return Err(format!("there is no cycle {}", args.cycle));
            };
            let at = ItemRef {
                cycle: cycle.name(),
                item: args.item,
            };
            let Some(item) = plan.item(&at) else {
                return Err(format!("there is no item {at}; write it with work_item first"));
            };
            match &item.status {
                Status::Running => {
                    let task = item.task.map(|t| format!(" (task {t})")).unwrap_or_default();
                    return Err(format!("{at} is running{task} already"));
                }
                Status::Done | Status::Failed => {
                    return Err(format!(
                        "{at} is closed as {}; write it again with work_item if there is more to do",
                        item.status
                    ));
                }
                Status::Broken(why) => {
                    return Err(format!(
                        "{at} does not read as an item ({why}); write it again with work_item"
                    ));
                }
                Status::Pending | Status::Returned => {}
            }
            if let Some(first) = plan.first_open_cycle()
                && first.number < cycle.number
            {
                return Err(format!(
                    "cycle {} still has open items: {}. Cycles run one after another: close them first",
                    first.name(),
                    open_items(first)
                ));
            }
            let path = at.path(store.dir());
            // A fresh subagent's work_todo, which learns its item below.
            let (id, todo) = match continued {
                Some(id) => {
                    if live.worked.get(&path) != Some(&id) {
                        return Err(format!(
                            "subagent {id} did not work on {at} in this run of nth; leave task_id out to start a fresh one"
                        ));
                    }
                    match subagents.describe(id) {
                        None => {
                            return Err(format!(
                                "subagent {id} has ended; leave task_id out to start a fresh one"
                            ));
                        }
                        Some((other, _)) if other != agent.name => {
                            return Err(format!(
                                "subagent {id} is @{other}, not @{}; leave task_id out to start a fresh one",
                                agent.name
                            ));
                        }
                        Some(_) => {}
                    }
                    if subagents.is_running(id) || subagents.queued(id) > 0 {
                        return Err(format!("subagent {id} is busy; wait for it, or stop it with task_stop"));
                    }
                    shared.spawner.read_only(id, planning)?;
                    (id, None)
                }
                None => {
                    let cell = Arc::new(OnceLock::new());
                    let todo: Box<dyn Tool> = Box::new(WorkTodo::new(cell.clone()));
                    let id = shared
                        .spawner
                        .spawn(agent, &format!("work {at}"), ctx, vec![todo]);
                    (id, Some(cell))
                }
            };
            let assignment = Assignment {
                files: shared.files.clone(),
                store: store.clone(),
                at: at.clone(),
                task: id,
                agent: agent.name.clone(),
                inbox: shared.inbox.clone(),
                closed: subagents.closed(id).unwrap_or_default(),
            };
            // Set before the subagent's first prompt is sent, so its tool
            // never finds it empty.
            if let Some(todo) = todo {
                let _ = todo.set(assignment.clone());
            }
            let before = item.clone();
            // `0` is no limit, as in the config.
            let timeout = match args.timeout_ms {
                Some(0) => None,
                Some(ms) => Some(Duration::from_millis(ms)),
                None => shared.spawner.limits().timeout,
            };
            let limit = match timeout {
                Some(timeout) => format!(
                    "You have {} minutes. Past that your turn is stopped and the item returns as timed out, with the steps you ticked.",
                    timeout.as_millis().div_ceil(60_000)
                ),
                None => String::new(),
            };
            let mut text = ASSIGNED
                .replace("{item}", &at.to_string())
                .replace("{limit}", &limit)
                .replace("{body}", &before.body());
            if planning {
                text = format!("{text}\n\n{PLANNING}");
            }
            let running = super::Item {
                status: Status::Running,
                agent: Some(agent.name.clone()),
                task: Some(id),
                assigned: Some(jiff::Timestamp::now()),
                outcome: None,
                result: None,
                note: None,
                ..before.clone()
            };
            // With an inbox every change to the item wakes the model.
            // Headless there is none, so the call waits for the notice.
            let headless = !shared.inbox.reaches_model();
            let (reply, waiting) = match headless {
                true => {
                    let (reply, waiting) = oneshot::channel();
                    (Some(reply), Some(waiting))
                }
                false => (None, None),
            };
            let assigned = Assigned { assignment, reply };
            let cancel = CancellationToken::new();
            let job = Job {
                text,
                cancel: cancel.clone(),
                done: Done::Work(assigned),
                timeout,
                // Headless nobody wakes for a stall.
                stall: shared.spawner.limits().stall.filter(|_| !headless),
            };
            // The file says running before the job goes out, all under the
            // lock the job's settling takes: a write that fails starts no
            // job, so the model hears one error and no return after it. A
            // call dropped mid-write, or a restore that fails, can leave the
            // file running for nobody, which the next work tool call settles
            // as left by a gone run.
            store
                .write(&running)
                .await
                .map_err(|e| format!("writing {at} failed: {e}"))?;
            if !subagents.prompt(id, job) {
                let restored = match store.write(&before).await {
                    Ok(()) => String::new(),
                    Err(e) => format!(" (and putting {at} back failed: {e})"),
                };
                return Err(format!(
                    "subagent {id} has ended{restored}; leave task_id out to start a fresh one"
                ));
            }
            live.running.insert(path.clone(), id);
            live.worked.insert(path.clone(), id);
            let Some(waiting) = waiting else {
                let verb = match continued {
                    Some(_) => format!("Sent {at} back to subagent {id} (@{})", agent.name),
                    None => format!("Assigned {at} to subagent {id} (@{})", agent.name),
                };
                return Ok(format!(
                    "{verb}. Its answer and any stall reach you as <work_item> notices, its todo updates along with them; carry on, or end your turn to wait. task_id {id} continues the same subagent on this item later."
                ));
            };
            // The subagent's work_todo and its settling take the lock.
            drop(live);
            // Dropped with this future when the parent's turn is cancelled,
            // which stops the subagent; the actor still settles the item.
            let _guard = cancel.drop_guard();
            match waiting.await {
                Ok(notice) => Ok(notice.render()),
                Err(_) => Err(format!("subagent {id} ended without settling {at}")),
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests;
