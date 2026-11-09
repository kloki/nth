//! The task that owns a subagent's session and runs its prompts one after
//! another, reporting to the front-end and settling each job's `Done`. It
//! alone holds the jobs waiting and the running one's token: the registry
//! reaches it through [`Command`]s and reads what it publishes as a
//! [`Status`].

use std::{collections::VecDeque, sync::Arc, time::Duration};

use nth_protocol::{Event, Message, Provider, Tool};
use tokio::{
    sync::{mpsc, oneshot, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use super::{
    Done, Job, Parent, State, Status, SubagentEvent, SubagentId, TaskNotice, TaskOutcome,
    TaskStall, task_outcome,
};
use crate::{Error, FrontEnd, Session, Spend, agent_loop::settle_unanswered};

/// How long a turn has to end once it was cancelled, by its budget, a stop
/// or a front-end that is gone, before the actor drops it. Cancelling is
/// cooperative: anything in the turn that awaits without heeding the cancel
/// would otherwise keep the job, and a work item on it, running for good.
pub(super) const GRACE: Duration = Duration::from_secs(30);

/// What the registry asks of an actor. The registry dropping its sender
/// is the last word: the child was forgotten with its session, so the
/// actor stops its turn, settles what waits as interrupted, tells no model
/// and ends.
pub(super) enum Command {
    Prompt(Box<Job>),
    /// Stops the running turn and every job waiting; the subagent stays.
    /// Answered with whether anything ran or waited.
    Stop(oneshot::Sender<bool>),
}

pub(super) struct Actor {
    pub id: SubagentId,
    pub agent: String,
    pub description: String,
    pub session: Session,
    pub provider: Arc<dyn Provider>,
    pub tools: Vec<Arc<dyn Tool>>,
    /// `None` without a front-end: nobody watches the tab.
    pub front_end: Option<mpsc::Sender<SubagentEvent>>,
    pub commands: mpsc::UnboundedReceiver<Command>,
    pub status: watch::Sender<Status>,
    /// Where its notices and spend go, while its session was not left.
    pub parent: Parent,
}

/// Who a job's ending is told as, and through whom.
struct Who {
    id: SubagentId,
    agent: String,
    description: String,
    parent: Parent,
}

/// The jobs an actor holds, and what it publishes about them.
struct Queue {
    commands: mpsc::UnboundedReceiver<Command>,
    waiting: VecDeque<Job>,
    /// Taken off `waiting` by a stop, to settle as interrupted once no turn
    /// runs: settling a work item waits on the lock the running turn's
    /// `work_todo` may hold, and the turn is not polled while an arm of
    /// its watch awaits.
    stopped: Vec<Job>,
    /// `false` once the registry let go of the child, or the front-end is
    /// gone: nothing more will run.
    open: bool,
    state: Option<State>,
    status: watch::Sender<Status>,
}

impl Queue {
    fn new(commands: mpsc::UnboundedReceiver<Command>, status: watch::Sender<Status>) -> Self {
        Self {
            commands,
            waiting: VecDeque::new(),
            stopped: Vec::new(),
            open: true,
            state: None,
            status,
        }
    }

    /// Acts on `command`, `running` being the running turn's token while
    /// one runs.
    fn take(&mut self, command: Command, running: Option<&CancellationToken>) {
        match command {
            Command::Prompt(job) => {
                self.waiting.push_back(*job);
                self.publish();
            }
            Command::Stop(answer) => {
                let anything = running.is_some_and(|token| !token.is_cancelled())
                    || self.waiting.iter().any(|job| !job.cancel.is_cancelled());
                self.stop(running);
                // Before answering, so whoever hears it reads the queue
                // emptied.
                self.publish();
                // A dropped receiver is a caller that stopped listening.
                let _ = answer.send(anything);
            }
        }
    }

    /// The registry let go of the child: forgotten with its session.
    fn close(&mut self, running: Option<&CancellationToken>) {
        self.open = false;
        self.stop(running);
        self.publish();
    }

    fn stop(&mut self, running: Option<&CancellationToken>) {
        if let Some(token) = running {
            token.cancel();
        }
        self.stopped.extend(self.waiting.drain(..));
    }

    /// Takes what was sent while the actor was busy elsewhere, so a stop
    /// that came then still catches the jobs waiting behind it.
    fn take_sent(&mut self) {
        while self.open {
            match self.commands.try_recv() {
                Ok(command) => self.take(command, None),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => self.close(None),
            }
        }
    }

    /// Waits for the next command; `false` once the registry let go.
    async fn wait(&mut self) -> bool {
        match self.commands.recv().await {
            Some(command) => self.take(command, None),
            None => self.close(None),
        }
        self.open
    }

    fn publish(&self) {
        let now = Status {
            state: self.state,
            queued: self
                .waiting
                .iter()
                .filter(|job| !job.cancel.is_cancelled())
                .count(),
        };
        self.status.send_if_modified(|status| {
            let changed = *status != now;
            *status = now;
            changed
        });
    }
}

/// Runs until the registry lets go of the child or the front-end is gone.
pub(super) async fn run(actor: Actor) {
    let Actor {
        id,
        agent,
        description,
        mut session,
        provider,
        tools,
        front_end,
        commands,
        status,
        parent,
    } = actor;
    let who = Who {
        id,
        agent,
        description,
        parent,
    };
    let mut queue = Queue::new(commands, status);
    let started = SubagentEvent::Started {
        id,
        agent: who.agent.clone(),
        description: who.description.clone(),
        model: session.model.clone(),
    };
    if !send(&front_end, started).await {
        abandon(&mut queue, &who).await;
        return;
    }
    loop {
        queue.take_sent();
        while !queue.stopped.is_empty() {
            who.settle_stopped(&mut queue).await;
            // A stop that came while settling still skips what waits,
            // rather than interrupting it once it has started.
            queue.take_sent();
        }
        let Some(job) = queue.waiting.pop_front() else {
            if queue.open && queue.wait().await {
                continue;
            }
            who.settle_stopped(&mut queue).await;
            return;
        };
        let Job {
            text,
            cancel,
            done,
            timeout,
            stall,
        } = job;
        // Stopped while it waited: it never ran, so nothing to show.
        if cancel.is_cancelled() {
            who.settle(done, Err(Error::Interrupted)).await;
            continue;
        }
        queue.state = Some(State::Running);
        queue.publish();
        if !send(
            &front_end,
            SubagentEvent::Prompted {
                id,
                text: text.clone(),
            },
        )
        .await
        {
            who.settle(done, Err(Error::Interrupted)).await;
            abandon(&mut queue, &who).await;
            return;
        }
        let began = Instant::now();
        let spends = session.usage.spends().len();
        let (tx, mut rx) = mpsc::channel::<Event>(256);
        // Nobody at the tab answers questions or looks at panels, and a
        // monitor's notices would go to the parent's model: headless.
        let front = FrontEnd::default();
        // The actor aborted mid-turn, as dropping the registry does, still
        // stops what the turn started that heeds its cancel.
        let stop_on_drop = cancel.clone().drop_guard();
        let mut watch = Watch {
            who: &who,
            done: &done,
            front_end: &front_end,
            cancel: &cancel,
            queue: &mut queue,
            timeout,
            stall,
            grace: GRACE,
            heard: true,
            timed_out: false,
            outbox: VecDeque::new(),
        };
        let turn = session.prompt(text, provider.as_ref(), &tools, &front, &tx, &cancel);
        let ended = watch.turn(turn, &mut rx).await;
        let Watch {
            mut heard,
            timed_out,
            mut outbox,
            ..
        } = watch;
        stop_on_drop.disarm();
        let result = match ended {
            Some(result) => result,
            None => {
                give_up(&mut session, spends);
                Err(Error::Interrupted)
            }
        };
        // Its own cancel is what ran out, not a stop: the parent hears why.
        let result = match (result, timeout) {
            (Err(Error::Interrupted), Some(timeout)) if timed_out => Err(Error::TimedOut(timeout)),
            (result, _) => result,
        };
        who.parent
            .record(session.usage.since(spends).iter().map(|spend| Spend {
                agent: Some(who.agent.clone()),
                ..spend.clone()
            }));
        let answer = result.map(|()| last_answer(&session));
        let outcome = task_outcome(&answer);
        // First, so a front-end that falls behind cannot keep whoever
        // waits from hearing; and before the turn counts as ended, so
        // whoever looks for the answer once it has, on `TurnEnded` or when
        // nothing runs any more, finds it.
        who.settle(done, answer).await;
        who.settle_stopped(&mut queue).await;
        // Sent just before the turn returned, and they belong before the
        // footer.
        if heard {
            heard = flush(&front_end, &mut outbox, &mut rx, id).await;
        }
        queue.state = Some(match &outcome {
            TaskOutcome::Completed(_) => State::Idle,
            TaskOutcome::Interrupted => State::Interrupted,
            TaskOutcome::Failed(_) => State::Failed,
        });
        queue.publish();
        let ended = SubagentEvent::TurnEnded {
            id,
            outcome,
            elapsed: began.elapsed(),
            model: session.model.clone(),
        };
        heard = heard && send(&front_end, ended).await;
        if !heard {
            abandon(&mut queue, &who).await;
            return;
        }
    }
}

/// What the actor watches a turn with, and what it learned doing so.
struct Watch<'a> {
    who: &'a Who,
    done: &'a Done,
    front_end: &'a Option<mpsc::Sender<SubagentEvent>>,
    cancel: &'a CancellationToken,
    /// Read while the turn runs, so a prompt queues, a stop stops it and
    /// a forgotten child's turn ends without waiting for it.
    queue: &'a mut Queue,
    timeout: Option<Duration>,
    stall: Option<Duration>,
    grace: Duration,
    /// `false` once the front-end is gone.
    heard: bool,
    /// The budget ran out, rather than someone stopping the turn.
    timed_out: bool,
    /// Events waiting for room on the front-end's channel rather than in a
    /// send the actor awaits, so a front-end that falls behind never stops
    /// the clocks; while one waits, the turn's own channel fills and holds
    /// the turn back instead.
    outbox: VecDeque<SubagentEvent>,
}

impl Watch<'_> {
    /// Runs `turn` until it ends, forwarding its events, reporting its
    /// silences, taking commands and holding it to its budget; `None` when
    /// it was dropped for not ending within the grace after its cancel.
    async fn turn(
        &mut self,
        turn: impl Future<Output = Result<(), Error>>,
        rx: &mut mpsc::Receiver<Event>,
    ) -> Option<Result<(), Error>> {
        tokio::pin!(turn);
        let deadline = budget(self.timeout);
        tokio::pin!(deadline);
        // A `Sleep`, not a one-off future like the deadline: it is pushed
        // back with every event, and again each time it fires, so a
        // subagent that stays silent is told about again.
        let quiet = tokio::time::sleep(self.stall.unwrap_or_default());
        tokio::pin!(quiet);
        // Armed once the turn is cancelled.
        let grace_over = tokio::time::sleep(self.grace);
        tokio::pin!(grace_over);
        // Cancelled, by any hand: the grace runs from here.
        let mut stopping = false;
        let mut silent_since = Instant::now();
        loop {
            tokio::select! {
                // An answer beats a grace that ran out in the same poll; the
                // clocks come before the events, which a busy turn could
                // otherwise keep ready on every pass.
                biased;
                result = &mut turn => return Some(result),
                // It ignored its cancel: dropped with this future, and what
                // it left open is the caller's to answer.
                () = &mut grace_over, if stopping => return None,
                // Cancelling is cooperative: the turn still ends on its own,
                // with every call answered, and the session stays valid to
                // continue from.
                () = self.cancel.cancelled(), if !stopping => {
                    stopping = true;
                    grace_over.as_mut().reset(Instant::now() + self.grace);
                }
                () = &mut deadline, if !stopping => {
                    self.timed_out = true;
                    self.cancel.cancel();
                }
                // Nobody is left to hear it: the turn stops at once rather
                // than run its budget out for nobody.
                () = gone(self.front_end), if self.heard => {
                    self.heard = false;
                    self.outbox.clear();
                    self.cancel.cancel();
                }
                command = self.queue.commands.recv(), if self.queue.open => match command {
                    Some(command) => self.queue.take(command, Some(self.cancel)),
                    None => self.queue.close(Some(self.cancel)),
                },
                () = &mut quiet, if self.stall.is_some() && !stopping => {
                    quiet.as_mut().reset(Instant::now() + self.stall.unwrap_or_default());
                    // It spoke; only the front-end has not kept up.
                    if !rx.is_empty() {
                        silent_since = Instant::now();
                        continue;
                    }
                    self.stalled(silent_since.elapsed());
                }
                room = room(self.front_end), if !self.outbox.is_empty() => match room {
                    Some(permit) => {
                        if let Some(event) = self.outbox.pop_front() {
                            permit.send(event);
                        }
                    }
                    // `gone` sees to the rest.
                    None => self.outbox.clear(),
                },
                Some(event) = rx.recv(), if self.outbox.is_empty() => {
                    silent_since = Instant::now();
                    if let Some(stall) = self.stall {
                        quiet.as_mut().reset(silent_since + stall);
                    }
                    self.forward(SubagentEvent::Session { id: self.who.id, event });
                }
            }
        }
    }

    /// The turn has said nothing for `quiet`: whoever waits on the job
    /// hears, unless the child was forgotten, and so does the tab.
    fn stalled(&mut self, quiet: Duration) {
        self.who.report_stall(self.done, quiet);
        self.forward(SubagentEvent::Stalled {
            id: self.who.id,
            quiet,
        });
    }

    fn forward(&mut self, event: SubagentEvent) {
        if self.heard && self.front_end.is_some() {
            self.outbox.push_back(event);
        }
    }
}

/// What a turn dropped part way leaves, mended: the calls it left open are
/// answered as a cancel would, so the session stays valid to continue
/// from, and the spend it opened at `at` is closed, kept if it counted
/// anything.
fn give_up(session: &mut Session, at: usize) {
    settle_unanswered(&mut session.messages);
    session.usage.close(at);
}

impl Who {
    /// Tells whoever waits on the job that its turn has said nothing for
    /// `quiet`. Only a model hears it, and only one whose session was not
    /// left: the tab has its own event, and nothing would wake a headless
    /// parent.
    fn report_stall(&self, done: &Done, quiet: Duration) {
        match done {
            Done::Work(assigned) => assigned.stalled(quiet),
            Done::Notify(inbox) => {
                self.parent.tell(|| {
                    inbox.post_task_stalled(TaskStall {
                        id: self.id,
                        agent: self.agent.clone(),
                        description: self.description.clone(),
                        quiet,
                    });
                });
            }
            Done::Reply(_) | Done::Nothing => {}
        }
    }

    /// Tells whoever waits on the job how it ended. A model whose session
    /// was left hears nothing: the one after it never asked. A work item's
    /// file still returns, or it would read as running forever.
    async fn settle(&self, done: Done, turn: super::Answer) {
        match done {
            // A dropped receiver is a parent that stopped waiting, which is fine.
            Done::Reply(reply) => {
                let _ = reply.send(turn);
            }
            Done::Notify(inbox) => {
                self.parent.tell(|| {
                    inbox.post_task(TaskNotice {
                        id: self.id,
                        agent: self.agent.clone(),
                        description: self.description.clone(),
                        outcome: task_outcome(&turn),
                    });
                });
            }
            Done::Work(assigned) => assigned.settle(turn).await,
            Done::Nothing => {}
        }
    }

    /// Settles the jobs a stop took off the queue as interrupted.
    async fn settle_stopped(&self, queue: &mut Queue) {
        for job in std::mem::take(&mut queue.stopped) {
            self.settle(job.done, Err(Error::Interrupted)).await;
        }
    }
}

/// Sends what waits in `outbox`, then what the turn sent just before it
/// ended; `false` once the front-end is gone.
async fn flush(
    front_end: &Option<mpsc::Sender<SubagentEvent>>,
    outbox: &mut VecDeque<SubagentEvent>,
    rx: &mut mpsc::Receiver<Event>,
    id: SubagentId,
) -> bool {
    let waiting = std::mem::take(outbox).into_iter();
    let late =
        std::iter::from_fn(|| rx.try_recv().ok()).map(|event| SubagentEvent::Session { id, event });
    for event in waiting.chain(late.collect::<Vec<_>>()) {
        if !send(front_end, event).await {
            return false;
        }
    }
    true
}

/// The front-end is gone, so the actor ends: every job waiting, or sent
/// before the registry hears, settles as interrupted, so whoever waits on
/// one hears, and a work item on one does not stay running. Nothing reads
/// as running or waiting after, since nothing will run.
async fn abandon(queue: &mut Queue, who: &Who) {
    // Closed first, so a prompt sent from now on is refused rather than
    // left unsettled.
    queue.commands.close();
    while let Some(command) = queue.commands.recv().await {
        queue.take(command, None);
    }
    if queue.state == Some(State::Running) {
        queue.state = Some(State::Interrupted);
    }
    queue.close(None);
    who.settle_stopped(queue).await;
}

/// Over once a job's time is up; never, for a job without a budget.
async fn budget(timeout: Option<Duration>) {
    match timeout {
        Some(timeout) => tokio::time::sleep(timeout).await,
        None => std::future::pending().await,
    }
}

/// The model's last answer: the message that ended the turn, since a turn
/// ends when the model answers without tools.
fn last_answer(session: &Session) -> String {
    session
        .messages
        .iter()
        .rev()
        .find_map(|message| match message {
            Message::Assistant(reply) => Some(reply.text.trim().to_string()),
            _ => None,
        })
        .unwrap_or_default()
}

/// `false` once the front-end is gone, when the actor should end: nobody
/// is left to hear from it. Without a front-end there is nothing to send.
async fn send(front_end: &Option<mpsc::Sender<SubagentEvent>>, event: SubagentEvent) -> bool {
    match front_end {
        Some(front_end) => front_end.send(event).await.is_ok(),
        None => true,
    }
}

/// Room for one event on the front-end's channel; `None` once it is gone.
/// Never, without a front-end.
async fn room(
    front_end: &Option<mpsc::Sender<SubagentEvent>>,
) -> Option<mpsc::Permit<'_, SubagentEvent>> {
    match front_end {
        Some(front_end) => front_end.reserve().await.ok(),
        None => std::future::pending().await,
    }
}

/// Over once the front-end is gone; never, without one.
async fn gone(front_end: &Option<mpsc::Sender<SubagentEvent>>) {
    match front_end {
        Some(front_end) => front_end.closed().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::StreamEvent;
    use tokio::sync::oneshot;

    use super::*;
    use crate::{
        Inbox,
        agent_loop::tests::{Reply, Scripted, call},
        inbox::quiet_text,
        subagent::{
            Subagents,
            tests::{explore, job, session},
        },
    };

    #[tokio::test(start_paused = true)]
    async fn a_silent_task_is_reported_stalled_and_again_while_it_stays_silent() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let inbox = Inbox::new();
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "find tabs",
            session(),
            Arc::new(Scripted::stalled()),
            Vec::new(),
        );
        let (mut silent, _) = job("go", Done::Notify(inbox.clone()));
        silent.stall = Some(Duration::from_secs(60));
        subagents.prompt(id, silent);

        let mut quiets = Vec::new();
        while quiets.len() < 2 {
            match rx.recv().await.expect("the subagent runs on") {
                SubagentEvent::Stalled { quiet, .. } => quiets.push(quiet),
                SubagentEvent::TurnEnded { .. } => panic!("the turn hangs"),
                _ => {}
            }
        }

        assert!(quiets[0] >= Duration::from_secs(60), "{quiets:?}");
        assert!(quiets[1] > quiets[0], "the silence grows: {quiets:?}");
        assert_eq!(
            inbox.take_notices().unwrap(),
            format!(
                "<task id=\"1\" agent=\"explore\" description=\"find tabs\" state=\"stalled\" quiet=\"{}\"/>",
                quiet_text(quiets[1])
            ),
            "one notice, the latest silence"
        );
        assert!(subagents.is_running(id), "a stall stops nothing");
    }

    /// The jobs of an actor whose registry still holds it, and so goes
    /// on sending.
    fn queue() -> (mpsc::UnboundedSender<Command>, Queue) {
        let (commands, received) = mpsc::unbounded_channel();
        let (status, _) = watch::channel(Status::default());
        (commands, Queue::new(received, status))
    }

    fn who() -> Who {
        Who {
            id: 1,
            agent: "explore".into(),
            description: "d".into(),
            parent: Subagents::default().reserve().parent(),
        }
    }

    /// A turn's watch for `done`, with no budget or stall and a short
    /// grace, as each test sets.
    fn watch<'a>(
        who: &'a Who,
        done: &'a Done,
        front_end: &'a Option<mpsc::Sender<SubagentEvent>>,
        queue: &'a mut Queue,
        cancel: &'a CancellationToken,
    ) -> Watch<'a> {
        Watch {
            who,
            done,
            front_end,
            cancel,
            queue,
            timeout: None,
            stall: None,
            grace: Duration::from_millis(10),
            heard: true,
            timed_out: false,
            outbox: VecDeque::new(),
        }
    }

    /// A turn that never ends and never heeds its cancel. Nothing in the
    /// loop awaits like this today: every await races the cancel, and a
    /// tool's call is dropped on it. The grace is for the day one does.
    fn deaf() -> impl Future<Output = Result<(), Error>> {
        std::future::pending()
    }

    #[tokio::test(start_paused = true)]
    async fn a_turn_that_does_not_stop_within_the_grace_is_given_up_as_timed_out() {
        let (who, done, cancel) = (who(), Done::Nothing, CancellationToken::new());
        let (_commands, mut queue) = queue();
        let mut watch = watch(&who, &done, &None, &mut queue, &cancel);
        watch.timeout = Some(Duration::from_millis(20));
        let (_tx, mut rx) = mpsc::channel(1);

        let ended = watch.turn(deaf(), &mut rx).await;

        assert!(ended.is_none(), "given up");
        assert!(watch.timed_out, "by its budget, so the job times out");
        assert!(cancel.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn a_stopped_turn_that_does_not_end_is_given_up_too() {
        let (who, done, cancel) = (who(), Done::Nothing, CancellationToken::new());
        let (_commands, mut queue) = queue();
        let mut watch = watch(&who, &done, &None, &mut queue, &cancel);
        // Runs out inside the grace: a stopped turn's budget no longer counts.
        watch.timeout = Some(Duration::from_millis(5));
        let (_tx, mut rx) = mpsc::channel(1);
        cancel.cancel();

        let ended = watch.turn(deaf(), &mut rx).await;

        assert!(ended.is_none(), "given up");
        assert!(!watch.timed_out, "stopped, not timed out");
    }

    #[tokio::test(start_paused = true)]
    async fn a_stopped_turn_is_never_reported_stalled() {
        let inbox = Inbox::new();
        let (who, done, cancel) = (who(), Done::Notify(inbox.clone()), CancellationToken::new());
        let (_commands, mut queue) = queue();
        let mut watch = watch(&who, &done, &None, &mut queue, &cancel);
        watch.stall = Some(Duration::from_millis(5));
        watch.grace = Duration::from_millis(50);
        let (_tx, mut rx) = mpsc::channel(1);
        cancel.cancel();

        let ended = watch.turn(deaf(), &mut rx).await;

        assert!(ended.is_none(), "given up");
        assert!(
            !inbox.has_notices(),
            "silent through the grace, not stalled"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_forgotten_task_that_stalls_tells_no_model() {
        let subagents = Subagents::default();
        let who = Who {
            parent: subagents.reserve().parent(),
            ..who()
        };
        subagents.forget_all();
        let inbox = Inbox::new();
        let (done, cancel) = (Done::Notify(inbox.clone()), CancellationToken::new());
        let (_commands, mut queue) = queue();
        let mut watch = watch(&who, &done, &None, &mut queue, &cancel);
        watch.stall = Some(Duration::from_millis(5));
        watch.timeout = Some(Duration::from_millis(20));
        let (_tx, mut rx) = mpsc::channel(1);

        let ended = watch.turn(deaf(), &mut rx).await;

        assert!(
            ended.is_none() && watch.timed_out,
            "it stalled, then ran out"
        );
        assert!(
            !inbox.has_notices(),
            "the session after the one that asked hears nothing"
        );
    }

    #[tokio::test]
    async fn dropping_the_registry_mid_turn_stops_the_turns_token() {
        let (tx, _rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "d",
            session(),
            Arc::new(Scripted::stalled()),
            Vec::new(),
        );
        let (hanging, cancel) = job("go", Done::Nothing);
        subagents.prompt(id, hanging);
        let mut status = subagents.status(id).expect("registered");
        status
            .wait_for(|s| s.state == Some(State::Running))
            .await
            .expect("the actor runs");

        drop(subagents);

        tokio::time::timeout(Duration::from_secs(5), cancel.cancelled())
            .await
            .expect("the aborted actor cancels what its turn started");
    }

    #[test]
    fn giving_up_answers_the_open_calls_and_closes_an_empty_spend() {
        let mut session = session();
        let at = session.usage.open(Spend::new("glm-5.3", None));
        session.messages.push(Message::User("go".into()));
        session
            .messages
            .push(Message::Assistant(nth_protocol::AssistantMessage {
                tool_calls: vec![call("1", "bash", "{}")],
                ..nth_protocol::AssistantMessage::default()
            }));

        give_up(&mut session, at);

        assert!(session.usage.spends().is_empty(), "nothing was spent");
        assert_eq!(
            session.messages.last(),
            Some(&Message::ToolResult {
                call_id: "1".into(),
                content: nth_protocol::failed(nth_protocol::INTERRUPTED),
                images: Vec::new(),
            })
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_front_end_that_falls_behind_is_no_stall() {
        let inbox = Inbox::new();
        let (who, done, cancel) = (who(), Done::Notify(inbox.clone()), CancellationToken::new());
        let (_commands, mut queue) = queue();
        // Full, and never read.
        let (front_tx, _front_rx) = mpsc::channel(1);
        front_tx
            .try_send(SubagentEvent::Prompted {
                id: 1,
                text: "go".into(),
            })
            .unwrap();
        let front_end = Some(front_tx);
        let mut watch = watch(&who, &done, &front_end, &mut queue, &cancel);
        watch.stall = Some(Duration::from_secs(10));
        watch.timeout = Some(Duration::from_secs(35));
        let (tx, mut rx) = mpsc::channel(8);
        for text in ["one", "two"] {
            tx.try_send(Event::TextDelta(text.into())).unwrap();
        }

        let ended = watch.turn(deaf(), &mut rx).await;

        assert!(ended.is_none() && watch.timed_out);
        assert!(!inbox.has_notices(), "it spoke; the tab only has not heard");
    }

    #[tokio::test]
    async fn a_front_end_that_reads_nothing_cannot_hold_a_job_past_its_budget() {
        let (tx, mut rx) = mpsc::channel(1);
        let subagents = Subagents::new(tx);
        let chatty = (0..1_000)
            .map(|i| StreamEvent::TextDelta(i.to_string()))
            .collect();
        let id = subagents.spawn(
            subagents.reserve(),
            &explore(),
            "d",
            session(),
            Arc::new(Scripted::always(Reply::Hangs(chatty))),
            Vec::new(),
        );
        let (reply, answer) = oneshot::channel();
        let (mut job, _) = job("go", Done::Reply(reply));
        job.timeout = Some(Duration::from_millis(20));
        subagents.prompt(id, job);
        // Started and Prompted, then never another.
        while !matches!(rx.recv().await, Some(SubagentEvent::Prompted { .. })) {}

        let answer = tokio::time::timeout(Duration::from_secs(5), answer)
            .await
            .expect("the job settles though the tab reads nothing");
        assert!(matches!(
            answer.unwrap(),
            Err(Error::TimedOut(timeout)) if timeout == Duration::from_millis(20)
        ));
    }

    #[tokio::test]
    async fn a_turn_is_stopped_once_the_front_end_is_gone() {
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
        let (reply, answer) = oneshot::channel();
        let (hanging, cancel) = job("go", Done::Reply(reply));
        subagents.prompt(id, hanging);
        while !matches!(rx.recv().await, Some(SubagentEvent::Prompted { .. })) {}

        drop(rx);

        let answer = tokio::time::timeout(Duration::from_secs(5), answer)
            .await
            .expect("the turn ends though its model never answers");
        assert!(matches!(answer.unwrap(), Err(Error::Interrupted)));
        assert!(cancel.is_cancelled(), "stopped, not left to run for nobody");
    }
}
