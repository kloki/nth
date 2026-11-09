//! Subagents: sessions the model delegates to with the task tool. Each runs
//! as its own actor, on its own `Session`, taking prompts from an inbox one
//! after another: the task tool's, and the ones you type into its tab. Its
//! events go to the front-end tagged with its id, and a task's answer goes
//! into the model's inbox as a notice, as a monitor's output does.
//!
//! `nth_context::agents` holds the definitions; this module runs them.

mod actor;
mod child;
mod mention;
mod notice;
mod parent;
mod spawn;
mod stop;
mod task;

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use actor::Command;
pub(crate) use child::child_session;
pub use mention::resolve;
pub use notice::{SubagentId, TaskNotice, TaskOutcome, TaskStall};
use nth_context::Agent;
use nth_protocol::{Event, Provider, Tool, Writable, tool_names};
pub(crate) use parent::Parent;
use parent::Spending;
pub(crate) use spawn::{Spawner, find_agent};
pub use stop::TaskStop;
pub(crate) use task::TaskIdArg;
pub use task::{Limits, Task};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::{Error, Inbox, Session, Spend};

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
    /// Nothing heard from its turn for `quiet`, though it still runs, and
    /// again while it stays silent; the model waiting on its task or work
    /// item was told, and a front-end wakes the model for it.
    Stalled { id: SubagentId, quiet: Duration },
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
pub type Answer = Result<String, Error>;

/// How a turn ended, as the model and the tab hear it.
pub(crate) fn task_outcome(turn: &Answer) -> TaskOutcome {
    match turn {
        Ok(text) => TaskOutcome::Completed(text.clone()),
        Err(Error::Interrupted) => TaskOutcome::Interrupted,
        Err(e) => TaskOutcome::Failed(e.to_string()),
    }
}

/// Who hears how a job ended.
pub enum Done {
    /// The task tool, waiting inline in a headless run.
    Reply(oneshot::Sender<Answer>),
    /// The model, through its inbox, as a `<task>` notice.
    Notify(Inbox),
    /// The work item it was assigned: its file returns with the outcome,
    /// and the model hears through its inbox as a `<work_item>` notice.
    Work(crate::work::Assigned),
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
    /// How long its turn may say nothing before whoever waits hears it
    /// stalled, and again every such interval while it stays silent. A
    /// prompt you type on the tab has none, since you are watching, and
    /// neither has a headless one, since nothing would wake its parent.
    pub stall: Option<Duration>,
}

/// Goes with every prompt from a planning parent: its child has no tool
/// that writes, but bash could still change files.
pub(crate) const PLANNING: &str = "<system-reminder>\nThe agent that sent you this task is planning and may not change the project. Do not change any file, with bash or otherwise: research, then report what you found.\n</system-reminder>";

/// The tools `agent` gets out of `all`: never the withheld ones, the
/// opt-in ones only when its own list names them, no writers while the
/// parent may only write its plan, and only the agent's own list when it
/// has one.
pub(crate) fn tools_for(
    all: &[Arc<dyn Tool>],
    agent: &Agent,
    writable: &Writable,
) -> Vec<Box<dyn Tool>> {
    let planning = writable.planning();
    all.iter()
        .filter(|tool| {
            let name = tool.spec().name;
            !task::WITHHELD.contains(&name)
                && (!task::OPT_IN.contains(&name) || agent.names(name))
                && !(planning && tool_names::writes_files(name))
                && agent.allows(name)
        })
        .map(|tool| Box::new(tool.clone()) as Box<dyn Tool>)
        .collect()
}

/// Where a subagent stands, as its actor last published it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Status {
    /// Where it is between prompts; `None` until its first one.
    pub state: Option<State>,
    /// Prompts waiting behind the running one, the stopped ones left out:
    /// they will be skipped. A prompt counts once the actor has read it,
    /// and a job whose own token is cancelled from outside stays counted
    /// until the actor next publishes.
    pub queued: usize,
}

/// A subagent's number, taken before it starts so what it is handed can
/// name it: `work_todo` is bound to its item and the id the item file
/// keeps. `Subagents::spawn` spends it; one never spent is a skipped
/// number, which nothing counts on.
#[must_use = "a reserved number is spent by Subagents::spawn"]
pub struct Reserved {
    parent: Parent,
}

impl std::fmt::Debug for Reserved {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Reserved").field(&self.id()).finish()
    }
}

impl Reserved {
    pub fn id(&self) -> SubagentId {
        self.parent.id()
    }

    /// How the subagent reaches its session's model, for a tool built
    /// before it starts: it hears when the subagent is forgotten.
    pub(crate) fn parent(&self) -> Parent {
        self.parent.clone()
    }
}

/// A stop [`Subagents::cancel`] sent. Dropping it leaves the stop sent.
pub struct Stop(Option<oneshot::Receiver<bool>>);

impl Stop {
    /// Whether anything ran or waited, as the actor found it once it read
    /// the stop; `false` too when it had ended.
    pub async fn anything(self) -> bool {
        match self.0 {
            Some(answered) => answered.await.unwrap_or(false),
            None => false,
        }
    }
}

/// The front-end's subagents, cheap to clone. Children run with or without
/// a front-end; without one, as in `nth run`, their events go nowhere.
#[derive(Clone, Default)]
pub struct Subagents(Arc<Inner>);

#[derive(Default)]
struct Inner {
    front_end: Option<mpsc::Sender<SubagentEvent>>,
    registry: Mutex<Registry>,
    /// What the children's finished turns spent, until the front-end
    /// takes it into the parent's session, and which children were
    /// forgotten. Apart from the registry so an actor can reach it without
    /// holding the registry alive.
    spending: Spending,
}

#[derive(Default)]
struct Registry {
    next: SubagentId,
    children: BTreeMap<SubagentId, Child>,
    /// The actors of forgotten children, which end on their own once
    /// their turn has; kept so dropping the last handle still stops them.
    leaving: Vec<JoinHandle<()>>,
}

/// What the registry answers for without asking the actor: the facts that
/// never change, and the handles to reach it.
struct Child {
    agent: String,
    description: String,
    session_id: uuid::Uuid,
    /// Whether its tools can change files, which a planning parent may not
    /// continue it with.
    writes: bool,
    /// Unbounded: the TUI sends from its synchronous loop and must never
    /// wait on an actor that may be waiting on the TUI. Prompts come at
    /// human speed, or one per task call. Dropped when the child is
    /// forgotten, which is what tells its actor.
    commands: mpsc::UnboundedSender<Command>,
    status: watch::Receiver<Status>,
    actor: JoinHandle<()>,
}

impl Subagents {
    /// Subagents whose events reach `front_end`.
    pub fn new(front_end: mpsc::Sender<SubagentEvent>) -> Self {
        Self(Arc::new(Inner {
            front_end: Some(front_end),
            registry: Mutex::default(),
            spending: Spending::default(),
        }))
    }

    /// The next subagent's number, for [`spawn`](Self::spawn).
    pub fn reserve(&self) -> Reserved {
        let mut registry = self.0.lock();
        registry.next += 1;
        Reserved {
            parent: self.0.spending.parent(registry.next),
        }
    }

    /// Starts a subagent of `agent` on `session` as the number `reserved`
    /// holds, announced as `Started` from its own task, and returns its
    /// id. One reserved before its session was left never starts: it is
    /// that session's, and a prompt to it is refused as to one that ended.
    pub fn spawn(
        &self,
        reserved: Reserved,
        agent: &Agent,
        description: &str,
        session: Session,
        provider: Arc<dyn Provider>,
        tools: Vec<Box<dyn Tool>>,
    ) -> SubagentId {
        let writes = tools
            .iter()
            .any(|tool| tool_names::writes_files(tool.spec().name));
        let Reserved { parent } = reserved;
        let id = parent.id();
        // Under the registry's lock, which forgetting takes too: either it
        // starts before and is forgotten with the rest, or it sees it was.
        let mut registry = self.0.lock();
        if self.0.spending.forgotten(id) {
            return id;
        }
        let (commands, inbox) = mpsc::unbounded_channel();
        let (publish, status) = watch::channel(Status::default());
        let session_id = session.id;
        let actor = tokio::spawn(actor::run(actor::Actor {
            id,
            agent: agent.name.clone(),
            description: description.to_string(),
            session,
            provider,
            tools,
            front_end: self.0.front_end.clone(),
            commands: inbox,
            status: publish,
            parent,
        }));
        registry.children.insert(
            id,
            Child {
                agent: agent.name.clone(),
                description: description.to_string(),
                session_id,
                writes,
                commands,
                status,
                actor,
            },
        );
        id
    }

    /// Queues `job` on subagent `id`; `false` when there is no such one,
    /// or it has ended.
    pub fn prompt(&self, id: SubagentId, job: Job) -> bool {
        let registry = self.0.lock();
        registry
            .children
            .get(&id)
            .is_some_and(|child| child.commands.send(Command::Prompt(Box::new(job))).is_ok())
    }

    /// Stops the running turn of subagent `id` and every prompt waiting
    /// for it, so stopping it leaves it idle. The stop goes out at once;
    /// what comes back says whether there was anything to stop, for
    /// whoever asks.
    pub fn cancel(&self, id: SubagentId) -> Stop {
        let (answer, answered) = oneshot::channel();
        let registry = self.0.lock();
        let sent = registry
            .children
            .get(&id)
            .is_some_and(|child| child.commands.send(Command::Stop(answer)).is_ok());
        Stop(sent.then_some(answered))
    }

    /// Ends every subagent, for a session that was left: their turns are
    /// stopped, what waits in their inboxes is dropped and nothing is
    /// posted to the model, so each actor ends on its own. What they spent
    /// and nobody took is dropped with them: it belonged to the session
    /// that was left.
    pub fn forget_all(&self) {
        let mut registry = self.0.lock();
        self.0.spending.forget_up_to(registry.next);
        let children = std::mem::take(&mut registry.children);
        registry.leaving.retain(|actor| !actor.is_finished());
        // Each child's command sender goes with it, which is what tells
        // its actor.
        registry
            .leaving
            .extend(children.into_values().map(|child| child.actor));
    }

    /// What the subagents' turns spent since this was last called, for
    /// the parent's session to [record](crate::Ledger::record).
    pub fn take_spent(&self) -> Vec<Spend> {
        self.0.spending.take()
    }

    /// The agent and description of subagent `id`.
    pub fn describe(&self, id: SubagentId) -> Option<(String, String)> {
        let registry = self.0.lock();
        let child = registry.children.get(&id)?;
        Some((child.agent.clone(), child.description.clone()))
    }

    /// The task number and agent name associated with a child session.
    pub fn describe_session(&self, session: &str) -> Option<(SubagentId, String)> {
        let id = session.parse::<uuid::Uuid>().ok()?;
        self.0.lock().children.iter().find_map(|(&number, child)| {
            (child.session_id == id).then(|| (number, child.agent.clone()))
        })
    }

    /// Whether subagent `id` belonged to a session that was left, for its
    /// events still on their way.
    pub fn forgotten(&self, id: SubagentId) -> bool {
        self.0.spending.forgotten(id)
    }

    /// How subagent `id` reaches its session's model, for what it posts
    /// outside its own turn's settling.
    pub(crate) fn parent(&self, id: SubagentId) -> Parent {
        self.0.spending.parent(id)
    }

    /// Whether subagent `id` has tools that change files.
    pub fn writes(&self, id: SubagentId) -> bool {
        self.0.lock().children.get(&id).is_some_and(|c| c.writes)
    }

    /// Where subagent `id` stands, as its actor last published it; a
    /// prompt or stop sent since counts once the actor has read it.
    pub fn status(&self, id: SubagentId) -> Option<watch::Receiver<Status>> {
        Some(self.0.lock().children.get(&id)?.status.clone())
    }

    pub fn state(&self, id: SubagentId) -> Option<State> {
        let registry = self.0.lock();
        registry.children.get(&id)?.status.borrow().state
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
            .filter(|child| child.status.borrow().state == Some(State::Running))
            .count()
    }

    /// Prompts waiting for subagent `id`, the stopped ones left out: they
    /// will be skipped. As [`Status::queued`]: a prompt counts once the
    /// actor has read it.
    pub fn queued(&self, id: SubagentId) -> usize {
        let registry = self.0.lock();
        registry
            .children
            .get(&id)
            .map_or(0, |child| child.status.borrow().queued)
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

/// The last handle gone, nobody could hear from the actors: stop them
/// rather than leave them to the runtime's shutdown. Each stops its
/// running turn's token as it is dropped.
impl Drop for Inner {
    fn drop(&mut self) {
        let registry = self
            .registry
            .get_mut()
            .expect("subagent registry lock poisoned");
        for child in registry.children.values() {
            child.actor.abort();
        }
        for actor in &registry.leaving {
            actor.abort();
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use nth_context::{Context, Paths};
    use nth_protocol::StreamEvent;

    use super::*;
    use crate::agent_loop::tests::{Reply, Scripted};

    pub(super) fn explore() -> Agent {
        Context::discover(std::path::Path::new("/nowhere"), &Paths::default())
            .agents
            .get("explore")
            .expect("built in")
            .clone()
    }

    pub(super) fn session() -> Session {
        Session::new("glm-5.3", "/repo".into()).as_subagent(None)
    }

    pub(crate) fn says(text: &str) -> Vec<StreamEvent> {
        vec![StreamEvent::TextDelta(text.into())]
    }

    pub(super) fn job(text: &str, done: Done) -> (Job, CancellationToken) {
        let cancel = CancellationToken::new();
        let job = Job {
            text: text.into(),
            cancel: cancel.clone(),
            done,
            timeout: None,
            stall: None,
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

    /// Waits until subagent `id`'s status reads `pick`.
    async fn status(subagents: &Subagents, id: SubagentId, pick: impl FnMut(&Status) -> bool) {
        let mut status = subagents.status(id).expect("registered");
        status.wait_for(pick).await.expect("the actor runs");
    }

    async fn running(subagents: &Subagents, id: SubagentId) {
        status(subagents, id, |s| s.state == Some(State::Running)).await;
    }

    /// Waits for the actors of the forgotten subagents to end by
    /// themselves.
    async fn left(subagents: &Subagents) {
        let leaving = std::mem::take(&mut subagents.0.lock().leaving);
        for actor in leaving {
            actor.await.expect("ended, not aborted");
        }
    }

    fn replies(
        subagents: &Subagents,
        id: SubagentId,
        texts: &[&str],
    ) -> Vec<oneshot::Receiver<Answer>> {
        texts
            .iter()
            .map(|text| {
                let (reply, answer) = oneshot::channel();
                assert!(subagents.prompt(id, job(text, Done::Reply(reply)).0));
                answer
            })
            .collect()
    }

    #[tokio::test]
    async fn status_follows_the_actor() {
        let (tx, _rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let provider = Arc::new(Scripted::script(vec![
            Reply::Hangs(Vec::new()),
            Reply::Events(says("two")),
            Reply::Events(says("three")),
        ]));
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "d",
            session(),
            provider,
            Vec::new(),
        );
        assert_eq!(
            subagents.status(id).map(|s| *s.borrow()),
            Some(Status::default())
        );
        let (one, stop_one) = job("one", Done::Nothing);
        subagents.prompt(id, one);
        running(&subagents, id).await;
        assert_eq!(subagents.queued(id), 0);

        let mut answers = replies(&subagents, id, &["two"]);
        let (skipped, cancel) = job("never", Done::Nothing);
        cancel.cancel();
        subagents.prompt(id, skipped);
        answers.extend(replies(&subagents, id, &["three"]));
        status(&subagents, id, |s| s.queued == 2).await;
        assert!(subagents.is_running(id), "one still runs");
        assert_eq!(subagents.queued(id), 2, "the cancelled one will not run");
        assert_eq!(subagents.running(), 1);

        stop_one.cancel();
        status(&subagents, id, |s| {
            s.state == Some(State::Idle) && s.queued == 0
        })
        .await;

        for (answer, text) in answers.into_iter().zip(["two", "three"]) {
            assert_eq!(answer.await.unwrap().unwrap(), text);
        }
        assert_eq!(subagents.running(), 0);
    }

    #[tokio::test]
    async fn a_stop_during_a_turn_drains_the_queue_and_the_subagent_goes_on() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let provider = Arc::new(Scripted::script(vec![
            Reply::Hangs(Vec::new()),
            Reply::Events(says("later")),
        ]));
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "d",
            session(),
            provider,
            Vec::new(),
        );
        let stopped = replies(&subagents, id, &["one", "two", "three"]);
        status(&subagents, id, |s| {
            s.state == Some(State::Running) && s.queued == 2
        })
        .await;

        assert!(
            subagents.cancel(id).anything().await,
            "one ran, two and three waited"
        );

        assert_eq!(subagents.queued(id), 0, "emptied once it answered");
        for answer in stopped {
            assert!(matches!(answer.await.unwrap(), Err(Error::Interrupted)));
        }
        status(&subagents, id, |s| s.state == Some(State::Interrupted)).await;
        let later = replies(&subagents, id, &["four"]);
        for answer in later {
            assert_eq!(answer.await.unwrap().unwrap(), "later", "it goes on");
        }
        let prompted: Vec<_> = until_ended(&mut rx)
            .await
            .into_iter()
            .chain(until_ended(&mut rx).await)
            .filter_map(|e| match e {
                SubagentEvent::Prompted { text, .. } => Some(text),
                _ => None,
            })
            .collect();
        assert_eq!(prompted, ["one", "four"], "two and three never ran");
    }

    #[tokio::test]
    async fn a_forgotten_childs_spend_is_dropped_by_its_tag() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let usage = |input| {
            StreamEvent::Usage(nth_protocol::Usage {
                input,
                ..nth_protocol::Usage::default()
            })
        };
        // Its spend is counted as the reply reports it, and recorded when
        // the turn ends: after its session was left.
        let old = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "d",
            session(),
            Arc::new(Scripted::script(vec![Reply::Hangs(vec![usage(300)])])),
            Vec::new(),
        );
        subagents.prompt(old, job("go", Done::Nothing).0);
        while !matches!(
            rx.recv().await,
            Some(SubagentEvent::Session {
                event: Event::Usage(_),
                ..
            })
        ) {}

        subagents.forget_all();
        left(&subagents).await;
        let new = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "d",
            session(),
            Arc::new(Scripted::new(vec![vec![
                StreamEvent::TextDelta("done".into()),
                usage(7),
            ]])),
            Vec::new(),
        );
        let answers = replies(&subagents, new, &["go"]);
        for answer in answers {
            answer.await.unwrap().unwrap();
        }

        let spent = subagents.take_spent();
        assert_eq!(spent.len(), 1, "{spent:?}");
        assert_eq!(spent[0].tokens.input, 7, "only the new session's");
    }

    #[tokio::test]
    async fn a_number_reserved_before_its_session_was_left_never_starts() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let reserved = subagents.reserve();
        let parent = reserved.parent();
        subagents.forget_all();
        let provider = Arc::new(Scripted::new(vec![says("late")]));

        let id = subagents.spawn(reserved, &explore(), "d", session(), provider, Vec::new());

        assert_eq!(id, 1);
        assert!(parent.forgotten(), "what it was handed hears it");
        assert!(subagents.ids().is_empty());
        let (late, _) = job("go", Done::Notify(Inbox::default()));
        assert!(!subagents.prompt(id, late), "refused as an ended one");
        assert!(rx.try_recv().is_err(), "no tab for it");
        assert_eq!(subagents.reserve().id(), 2, "numbers go on");
    }

    #[tokio::test]
    async fn a_task_runs_and_replies_with_the_answer() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let provider = Arc::new(Scripted::new(vec![says("in content.rs")]));
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "find tabs",
            session(),
            provider,
            Vec::new(),
        );
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
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "find tabs",
            session(),
            provider,
            Vec::new(),
        );
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
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "d",
            session(),
            provider,
            tools,
        );
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
    async fn a_finished_turn_hands_its_spend_to_the_parent_once() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let reply = vec![
            StreamEvent::TextDelta("done".into()),
            StreamEvent::Usage(nth_protocol::Usage {
                input: 300,
                output: 7,
                ..nth_protocol::Usage::default()
            }),
        ];
        let provider = Arc::new(Scripted::new(vec![reply]));
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "d",
            session(),
            provider,
            Vec::new(),
        );
        subagents.prompt(id, job("go", Done::Nothing).0);

        until_ended(&mut rx).await;

        let spent = subagents.take_spent();
        assert_eq!(spent.len(), 1);
        assert_eq!(spent[0].agent.as_deref(), Some("explore"));
        assert_eq!(spent[0].model, "glm-5.3");
        assert_eq!((spent[0].steps, spent[0].tokens.input), (1, 300));
        assert!(subagents.take_spent().is_empty(), "taken once");
    }

    #[tokio::test]
    async fn a_child_that_answers_nothing_fails_its_task() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let inbox = Inbox::new();
        let provider = Arc::new(Scripted::new(vec![vec![]]));
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "find tabs",
            session(),
            provider,
            Vec::new(),
        );
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
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "d",
            session(),
            provider,
            Vec::new(),
        );
        let (first_tx, first_rx) = oneshot::channel();
        let (skipped_tx, skipped_rx) = oneshot::channel();
        let (second_tx, second_rx) = oneshot::channel();
        let (skipped, cancel) = job("never", Done::Reply(skipped_tx));
        cancel.cancel();
        subagents.prompt(id, job("one", Done::Reply(first_tx)).0);
        subagents.prompt(id, skipped);
        subagents.prompt(id, job("two", Done::Reply(second_tx)).0);

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
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "d",
            session(),
            provider,
            Vec::new(),
        );
        let (reply_tx, reply_rx) = oneshot::channel();
        subagents.prompt(id, job("one", Done::Nothing).0);
        subagents.prompt(id, job("two", Done::Reply(reply_tx)).0);

        assert!(subagents.cancel(id).anything().await);

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
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "slow",
            session(),
            Arc::new(Scripted::stalled()),
            Vec::new(),
        );
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
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "slow",
            session(),
            Arc::new(Scripted::stalled()),
            Vec::new(),
        );
        let (reply_tx, reply_rx) = oneshot::channel();
        subagents.prompt(id, job("go", Done::Reply(reply_tx)).0);

        running(&subagents, id).await;
        assert!(subagents.is_running(id));
        assert!(subagents.cancel(id).anything().await);

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
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "d",
            session(),
            provider,
            Vec::new(),
        );
        subagents.prompt(id, job("one", Done::Notify(inbox.clone())).0);
        subagents.prompt(id, job("two", Done::Notify(inbox.clone())).0);

        subagents.forget_all();
        // Ended on its own, not aborted with the registry.
        left(&subagents).await;
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
    async fn abandoning_clears_what_waited() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "d",
            session(),
            Arc::new(Scripted::stalled()),
            Vec::new(),
        );
        let replies: Vec<_> = (0..3)
            .map(|i| {
                let (reply, waiting) = oneshot::channel();
                subagents.prompt(id, job(&format!("job {i}"), Done::Reply(reply)).0);
                waiting
            })
            .collect();
        while !matches!(rx.recv().await, Some(SubagentEvent::Prompted { .. })) {}
        assert_eq!(subagents.queued(id), 2);

        drop(rx);

        tokio::time::timeout(Duration::from_secs(5), async {
            for reply in replies {
                assert!(matches!(reply.await.unwrap(), Err(Error::Interrupted)));
            }
        })
        .await
        .expect("every job settles once the front-end is gone");
        assert_eq!(subagents.queued(id), 0, "nothing will run what waited");
        assert!(!subagents.is_running(id));
        assert!(
            !subagents.cancel(id).anything().await,
            "nothing left to stop"
        );
    }

    #[tokio::test]
    async fn forgetting_ends_the_actors_and_so_does_dropping_the_handle() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let provider = Arc::new(Scripted::new(vec![says("x")]));
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "d",
            session(),
            provider,
            Vec::new(),
        );
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
