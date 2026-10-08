//! Subagents: sessions the model delegates to with the task tool. Each runs
//! as its own actor, on its own `Session`, taking prompts from an inbox one
//! after another: the task tool's, and the ones you type into its tab. Its
//! events go to the front-end tagged with its id, and a task's answer goes
//! into the model's inbox as a notice, as a monitor's output does.
//!
//! `nth_context::agents` holds the definitions; this module runs them.

mod actor;
mod mention;
mod task;

/// Tools that change files, kept from a subagent while its parent plans.
pub(crate) const WRITERS: [&str; 3] = ["write", "edit", "apply_patch"];

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
    time::Duration,
};

pub use mention::resolve;
use nth_context::Agent;
use nth_protocol::{Event, Inbox, Provider, TaskId, TaskOutcome, Tool};
pub use task::{Limits, Task};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{Error, Session};

/// Numbered from 1 for as long as the front-end runs, like a monitor; it is
/// the `task_id` the model continues a subagent with.
pub type SubagentId = TaskId;

/// What a subagent reports to the front-end, for its tab.
#[derive(Debug, Clone, PartialEq)]
pub enum SubagentEvent {
    /// Always first, from the actor itself, so a tab exists before anything
    /// else arrives for it.
    Started {
        id: SubagentId,
        agent: String,
        description: String,
        model: String,
    },
    /// A prompt its turn starts on: the task tool's, or one you typed.
    Prompted { id: SubagentId, text: String },
    /// What its session reports while the turn runs.
    Session { id: SubagentId, event: Event },
    TurnEnded {
        id: SubagentId,
        outcome: TaskOutcome,
        elapsed: Duration,
        model: String,
    },
}

/// Where a subagent is between prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    /// Its last turn answered.
    Idle,
    /// Its last turn was stopped.
    Interrupted,
    /// Its last turn failed.
    Failed,
}

/// How a turn ended: the answer, or why there is none.
pub type Turn = Result<String, Error>;

/// Who hears how a job ended.
pub enum Done {
    /// The task tool, waiting inline in a headless run.
    Reply(oneshot::Sender<Turn>),
    /// The model, through its inbox, as a `<task>` notice.
    Notify(Inbox),
    /// Nobody but the tab: you typed the prompt and are looking at it.
    Nothing,
}

/// One prompt for a subagent. The caller mints `cancel`, so a job can be
/// stopped while it still waits in the inbox.
pub struct Job {
    pub text: String,
    pub cancel: CancellationToken,
    pub done: Done,
    /// How long its turn may run before it is stopped and fails with
    /// [`Error::TimedOut`]: the task tool's budget. A prompt you type on
    /// the tab has none, since you are watching.
    pub timeout: Option<Duration>,
}

/// What the registry and a subagent's actor share about it. The actor
/// updates it; the registry and the front-end read it.
#[derive(Debug, Default)]
pub(crate) struct ChildState {
    /// The running job's token, for ctrl+w and Esc on the tab.
    running: Option<CancellationToken>,
    /// The tokens of the jobs in the inbox the actor has not started, in
    /// the order they wait, so stopping the subagent stops them too.
    waiting: VecDeque<CancellationToken>,
    state: Option<State>,
}

/// The front-end's subagents, cheap to clone. Children run with or without
/// a front-end; without one, as in `nth run`, their events go nowhere.
#[derive(Clone, Default)]
pub struct Subagents(Arc<Inner>);

#[derive(Default)]
struct Inner {
    front_end: Option<mpsc::Sender<SubagentEvent>>,
    registry: Mutex<Registry>,
}

#[derive(Default)]
struct Registry {
    next: SubagentId,
    /// Every id up to this one was forgotten with its session.
    forgotten: SubagentId,
    children: BTreeMap<SubagentId, Child>,
}

struct Child {
    agent: String,
    description: String,
    /// Whether its tools can change files, which a planning parent may not
    /// continue it with.
    writes: bool,
    /// Cancelled when the child is forgotten: the actor stops whatever
    /// waits in its inbox and tells no model, since the session it would
    /// tell was left.
    closed: CancellationToken,
    /// Unbounded: the TUI sends from its synchronous loop and must never
    /// wait on an actor that may be waiting on the TUI. Prompts come at
    /// human speed, or one per task call.
    inbox: mpsc::UnboundedSender<Job>,
    actor: JoinHandle<()>,
    shared: Arc<Mutex<ChildState>>,
}

impl Subagents {
    /// Subagents whose events reach `front_end`.
    pub fn new(front_end: mpsc::Sender<SubagentEvent>) -> Self {
        Self(Arc::new(Inner {
            front_end: Some(front_end),
            registry: Mutex::default(),
        }))
    }

    /// Starts a subagent of `agent` on `session`, numbered and announced
    /// as `Started` from its own task, and returns its id.
    pub fn spawn(
        &self,
        agent: &Agent,
        description: &str,
        session: Session,
        provider: Arc<dyn Provider>,
        tools: Vec<Box<dyn Tool>>,
    ) -> SubagentId {
        let (inbox, jobs) = mpsc::unbounded_channel();
        let shared = Arc::new(Mutex::new(ChildState::default()));
        let writes = tools.iter().any(|tool| WRITERS.contains(&tool.spec().name));
        let closed = CancellationToken::new();
        let mut registry = self.0.lock();
        registry.next += 1;
        let id = registry.next;
        let actor = tokio::spawn(actor::run(actor::Actor {
            id,
            agent: agent.name.clone(),
            description: description.to_string(),
            session,
            provider,
            tools,
            front_end: self.0.front_end.clone(),
            shared: shared.clone(),
            closed: closed.clone(),
            jobs,
        }));
        registry.children.insert(
            id,
            Child {
                agent: agent.name.clone(),
                description: description.to_string(),
                writes,
                closed,
                inbox,
                actor,
                shared,
            },
        );
        id
    }

    /// Queues `job` on subagent `id`; `false` when there is no such one.
    pub fn prompt(&self, id: SubagentId, job: Job) -> bool {
        let registry = self.0.lock();
        let Some(child) = registry.children.get(&id) else {
            return false;
        };
        // Counted under the lock the actor takes to count it off, so it is
        // never taken off before it was put on.
        let mut state = child.lock();
        let token = job.cancel.clone();
        let sent = child.inbox.send(job).is_ok();
        if sent {
            state.waiting.push_back(token);
        }
        sent
    }

    /// Stops the running turn of subagent `id` and every prompt waiting
    /// for it, so stopping it leaves it idle; `false` when nothing ran or
    /// waited.
    pub fn cancel(&self, id: SubagentId) -> bool {
        let registry = self.0.lock();
        let Some(child) = registry.children.get(&id) else {
            return false;
        };
        let state = child.lock();
        let tokens: Vec<_> = state
            .running
            .iter()
            .chain(&state.waiting)
            .filter(|token| !token.is_cancelled())
            .collect();
        tokens.iter().for_each(|token| token.cancel());
        !tokens.is_empty()
    }

    /// Ends every subagent, for a session that was left: the running turns
    /// are stopped, what waits in the inboxes is dropped and nothing is
    /// posted to the model, so each actor ends on its own.
    pub fn forget_all(&self) {
        let mut registry = self.0.lock();
        registry.forgotten = registry.next;
        for (_, child) in std::mem::take(&mut registry.children) {
            // Before looking for the running turn: an actor that has not
            // yet marked one running sees `closed` and skips it.
            child.closed.cancel();
            if let Some(token) = &child.lock().running {
                token.cancel();
            }
        }
    }

    /// The agent and description of subagent `id`.
    pub fn describe(&self, id: SubagentId) -> Option<(String, String)> {
        let registry = self.0.lock();
        let child = registry.children.get(&id)?;
        Some((child.agent.clone(), child.description.clone()))
    }

    /// Whether subagent `id` belonged to a session that was left, for its
    /// events still on their way.
    pub fn forgotten(&self, id: SubagentId) -> bool {
        id <= self.0.lock().forgotten
    }

    /// Whether subagent `id` has tools that change files.
    pub fn writes(&self, id: SubagentId) -> bool {
        self.0.lock().children.get(&id).is_some_and(|c| c.writes)
    }

    pub fn state(&self, id: SubagentId) -> Option<State> {
        let registry = self.0.lock();
        registry.children.get(&id)?.lock().state
    }

    pub fn is_running(&self, id: SubagentId) -> bool {
        self.state(id) == Some(State::Running)
    }

    /// Subagents whose turn runs.
    pub fn running(&self) -> usize {
        let registry = self.0.lock();
        registry
            .children
            .values()
            .filter(|child| child.lock().state == Some(State::Running))
            .count()
    }

    /// Prompts waiting in the inbox of subagent `id`, the stopped ones
    /// left out: they will be skipped.
    pub fn queued(&self, id: SubagentId) -> usize {
        let registry = self.0.lock();
        registry.children.get(&id).map_or(0, |child| {
            let state = child.lock();
            state.waiting.iter().filter(|t| !t.is_cancelled()).count()
        })
    }

    pub fn ids(&self) -> Vec<SubagentId> {
        self.0.lock().children.keys().copied().collect()
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, Registry> {
        self.registry
            .lock()
            .expect("subagent registry lock poisoned")
    }
}

impl Child {
    fn lock(&self) -> std::sync::MutexGuard<'_, ChildState> {
        self.shared.lock().expect("subagent state lock poisoned")
    }
}

/// The last handle gone, nobody could hear from the actors: stop them
/// rather than leave them to the runtime's shutdown.
impl Drop for Inner {
    fn drop(&mut self) {
        let registry = self
            .registry
            .get_mut()
            .expect("subagent registry lock poisoned");
        for child in registry.children.values() {
            if let Some(token) = &child.lock().running {
                token.cancel();
            }
            child.actor.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use futures::{FutureExt, StreamExt, future::BoxFuture, stream::BoxStream};
    use nth_context::{Context, Paths};
    use nth_protocol::{BoxError, Listing, Request, StreamEvent};

    use super::*;
    use crate::agent_loop::tests::Scripted;

    /// A model whose reply never arrives.
    pub(super) struct Stalled;

    impl Provider for Stalled {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            async { Ok(Listing::default()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            async { Ok(futures::stream::pending().boxed()) }.boxed()
        }
    }

    fn explore() -> Agent {
        Context::discover(std::path::Path::new("/nowhere"), &Paths::default())
            .agents
            .get("explore")
            .expect("built in")
            .clone()
    }

    fn session() -> Session {
        Session::new("glm-5.3", "/repo".into()).as_subagent(None)
    }

    pub(super) fn says(text: &str) -> Vec<StreamEvent> {
        vec![StreamEvent::TextDelta(text.into())]
    }

    fn job(text: &str, done: Done) -> (Job, CancellationToken) {
        let cancel = CancellationToken::new();
        let job = Job {
            text: text.into(),
            cancel: cancel.clone(),
            done,
            timeout: None,
        };
        (job, cancel)
    }

    async fn until_ended(rx: &mut mpsc::Receiver<SubagentEvent>) -> Vec<SubagentEvent> {
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            let ended = matches!(event, SubagentEvent::TurnEnded { .. });
            events.push(event);
            if ended {
                break;
            }
        }
        events
    }

    #[tokio::test]
    async fn a_task_runs_and_replies_with_the_answer() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let provider = Arc::new(Scripted::new(vec![says("in content.rs")]));
        let id = subagents.spawn(&explore(), "find tabs", session(), provider, Vec::new());
        let (reply_tx, reply_rx) = oneshot::channel();
        let (first, _) = job("where do tabs open?", Done::Reply(reply_tx));
        assert!(subagents.prompt(id, first));

        let events = until_ended(&mut rx).await;

        assert_eq!(
            reply_rx.await.expect("replied").expect("answered"),
            "in content.rs"
        );
        assert_eq!(
            events[0],
            SubagentEvent::Started {
                id: 1,
                agent: "explore".into(),
                description: "find tabs".into(),
                model: "glm-5.3".into(),
            }
        );
        assert_eq!(
            events[1],
            SubagentEvent::Prompted {
                id: 1,
                text: "where do tabs open?".into()
            }
        );
        assert_eq!(
            events[2],
            SubagentEvent::Session {
                id: 1,
                event: Event::TextDelta("in content.rs".into())
            }
        );
        assert!(matches!(
            events.last(),
            Some(SubagentEvent::TurnEnded {
                id: 1,
                outcome: TaskOutcome::Completed(text),
                ..
            }) if text == "in content.rs"
        ));
        assert_eq!(subagents.state(id), Some(State::Idle));
        assert_eq!(subagents.running(), 0);
        assert_eq!(
            subagents.describe(id),
            Some(("explore".into(), "find tabs".into()))
        );
        assert!(
            !subagents.prompt(9, job("x", Done::Nothing).0),
            "no such subagent"
        );
    }

    #[tokio::test]
    async fn a_notified_task_posts_its_answer_in_the_models_inbox() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let inbox = Inbox::new();
        let provider = Arc::new(Scripted::new(vec![says("done")]));
        let id = subagents.spawn(&explore(), "find tabs", session(), provider, Vec::new());
        subagents.prompt(id, job("go", Done::Notify(inbox.clone())).0);

        until_ended(&mut rx).await;

        assert_eq!(
            inbox.take_notices().unwrap(),
            "<task id=\"1\" agent=\"explore\" description=\"find tabs\" state=\"completed\">\n<task_result>\ndone\n</task_result>\n</task>"
        );
    }

    /// A child has nobody to ask, so a loop stops it with a failed notice
    /// rather than running the same call for the whole step budget.
    #[tokio::test]
    async fn a_looping_child_fails_instead_of_asking() {
        struct Noop;
        impl Tool for Noop {
            fn spec(&self) -> nth_protocol::ToolSpec {
                nth_protocol::ToolSpec {
                    name: "noop",
                    description: String::new(),
                    parameters: serde_json::json!({}),
                }
            }
            fn call<'a>(
                &'a self,
                _: serde_json::Value,
                _: &'a nth_protocol::ToolContext,
            ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
                Box::pin(async { Ok(String::new()) })
            }
        }
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let inbox = Inbox::new();
        let same =
            |id: &str| StreamEvent::ToolCall(crate::agent_loop::tests::call(id, "noop", "{}"));
        let provider = Arc::new(Scripted::new(vec![vec![same("1"), same("2"), same("3")]]));
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Noop)];
        let id = subagents.spawn(&explore(), "d", session(), provider, tools);
        subagents.prompt(id, job("go", Done::Notify(inbox.clone())).0);

        until_ended(&mut rx).await;

        assert_eq!(subagents.state(id), Some(State::Failed));
        let notice = inbox.take_notices().unwrap();
        assert!(
            notice.contains("state=\"failed\"")
                && notice.contains("called noop with the same input"),
            "{notice}"
        );
    }

    #[tokio::test]
    async fn a_child_that_answers_nothing_fails_its_task() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let inbox = Inbox::new();
        let provider = Arc::new(Scripted::new(vec![vec![]]));
        let id = subagents.spawn(&explore(), "find tabs", session(), provider, Vec::new());
        subagents.prompt(id, job("go", Done::Notify(inbox.clone())).0);

        until_ended(&mut rx).await;

        assert_eq!(subagents.state(id), Some(State::Failed));
        assert_eq!(
            inbox.take_notices().unwrap(),
            "<task id=\"1\" agent=\"explore\" description=\"find tabs\" state=\"failed\">\n<task_error>\nthe model answered nothing\n</task_error>\n</task>"
        );
    }

    #[tokio::test]
    async fn jobs_run_one_after_another_and_a_cancelled_one_is_skipped() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let provider = Arc::new(Scripted::new(vec![says("first"), says("second")]));
        let id = subagents.spawn(&explore(), "d", session(), provider, Vec::new());
        let (first_tx, first_rx) = oneshot::channel();
        let (skipped_tx, skipped_rx) = oneshot::channel();
        let (second_tx, second_rx) = oneshot::channel();
        let (skipped, cancel) = job("never", Done::Reply(skipped_tx));
        cancel.cancel();
        subagents.prompt(id, job("one", Done::Reply(first_tx)).0);
        subagents.prompt(id, skipped);
        subagents.prompt(id, job("two", Done::Reply(second_tx)).0);
        assert_eq!(subagents.queued(id), 2, "the cancelled one will not run");

        assert_eq!(first_rx.await.unwrap().unwrap(), "first");
        assert!(matches!(skipped_rx.await.unwrap(), Err(Error::Interrupted)));
        assert_eq!(second_rx.await.unwrap().unwrap(), "second");
        let prompted: Vec<_> = until_ended(&mut rx)
            .await
            .into_iter()
            .chain(until_ended(&mut rx).await)
            .filter_map(|e| match e {
                SubagentEvent::Prompted { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(prompted, ["one", "two"], "the skipped job never started");
        assert_eq!(subagents.queued(id), 0);
    }

    #[tokio::test]
    async fn cancelling_stops_what_waits_too() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let provider = Arc::new(Scripted::new(vec![says("one"), says("two")]));
        let id = subagents.spawn(&explore(), "d", session(), provider, Vec::new());
        let (reply_tx, reply_rx) = oneshot::channel();
        subagents.prompt(id, job("one", Done::Nothing).0);
        subagents.prompt(id, job("two", Done::Reply(reply_tx)).0);

        assert!(subagents.cancel(id));

        assert_eq!(subagents.queued(id), 0);
        assert!(matches!(reply_rx.await.unwrap(), Err(Error::Interrupted)));
        drop(subagents);
        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SubagentEvent::Prompted { .. })),
            "neither ran: {events:?}"
        );
    }

    #[tokio::test]
    async fn a_job_past_its_budget_fails_with_why_and_the_subagent_can_go_on() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let inbox = Inbox::new();
        let id = subagents.spawn(&explore(), "slow", session(), Arc::new(Stalled), Vec::new());
        let (mut slow, _) = job("go", Done::Notify(inbox.clone()));
        slow.timeout = Some(Duration::from_millis(20));
        subagents.prompt(id, slow);

        let events = until_ended(&mut rx).await;

        assert!(
            matches!(
                events.last(),
                Some(SubagentEvent::TurnEnded {
                    outcome: TaskOutcome::Failed(why),
                    ..
                }) if why == "no answer after 20ms; its task_id continues it"
            ),
            "{events:?}"
        );
        assert_eq!(subagents.state(id), Some(State::Failed));
        assert!(
            inbox
                .take_notices()
                .unwrap()
                .contains("<task_error>\nno answer after 20ms"),
            "the parent hears why"
        );
        assert!(
            subagents.prompt(id, job("again", Done::Nothing).0),
            "the session is still there to continue"
        );
    }

    #[tokio::test]
    async fn a_job_without_a_budget_runs_until_stopped() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let id = subagents.spawn(&explore(), "slow", session(), Arc::new(Stalled), Vec::new());
        let (reply_tx, reply_rx) = oneshot::channel();
        subagents.prompt(id, job("go", Done::Reply(reply_tx)).0);

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(subagents.is_running(id));
        assert!(subagents.cancel(id));

        until_ended(&mut rx).await;
        assert!(matches!(reply_rx.await.unwrap(), Err(Error::Interrupted)));
        assert_eq!(subagents.state(id), Some(State::Interrupted));
    }

    #[tokio::test]
    async fn a_forgotten_subagent_runs_nothing_queued_and_tells_no_model() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let inbox = Inbox::new();
        let provider = Arc::new(Scripted::new(vec![says("one"), says("two")]));
        let id = subagents.spawn(&explore(), "d", session(), provider, Vec::new());
        subagents.prompt(id, job("one", Done::Notify(inbox.clone())).0);
        subagents.prompt(id, job("two", Done::Notify(inbox.clone())).0);

        subagents.forget_all();
        drop(subagents);

        let mut events = Vec::new();
        while let Some(event) = rx.recv().await {
            events.push(event);
        }
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, SubagentEvent::Prompted { .. })),
            "{events:?}"
        );
        assert!(!inbox.has_notices());
    }

    #[tokio::test]
    async fn forgetting_ends_the_actors_and_so_does_dropping_the_handle() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let provider = Arc::new(Scripted::new(vec![says("x")]));
        let id = subagents.spawn(&explore(), "d", session(), provider, Vec::new());
        assert_eq!(
            rx.recv()
                .await
                .map(|e| matches!(e, SubagentEvent::Started { .. })),
            Some(true)
        );

        subagents.forget_all();
        assert_eq!(subagents.ids(), Vec::<SubagentId>::new());
        assert!(!subagents.prompt(id, job("x", Done::Nothing).0));
        drop(subagents);

        // Every sender gone: the registry's with the handle, the actor's
        // when it ended on its closed inbox.
        assert_eq!(rx.recv().await, None);
    }
}
