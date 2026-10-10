//! The task that owns a subagent's session and runs its prompts one after
//! another, reporting to the front-end and settling each job's `Done`.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use nth_protocol::{Event, FrontEnd, Message, Provider, TaskNotice, TaskOutcome, Tool};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{ChildState, Done, Job, State, SubagentEvent, SubagentId};
use crate::{Error, Session, Spend};

pub(super) struct Actor {
    pub id: SubagentId,
    pub agent: String,
    pub description: String,
    pub session: Session,
    pub provider: Arc<dyn Provider>,
    pub tools: Vec<Box<dyn Tool>>,
    /// `None` without a front-end: nobody watches the tab.
    pub front_end: Option<mpsc::Sender<SubagentEvent>>,
    pub shared: Arc<Mutex<ChildState>>,
    /// The registry forgot the child.
    pub closed: CancellationToken,
    pub jobs: mpsc::UnboundedReceiver<Job>,
    /// Where each turn's spend goes, for the parent's session.
    pub spent: Arc<Mutex<Vec<Spend>>>,
}

/// Runs until the inbox closes or the front-end is gone.
pub(super) async fn run(actor: Actor) {
    let Actor {
        id,
        agent,
        description,
        mut session,
        provider,
        tools,
        front_end,
        shared,
        closed,
        mut jobs,
        spent,
    } = actor;
    let started = SubagentEvent::Started {
        id,
        agent: agent.clone(),
        description: description.clone(),
        model: session.model.clone(),
    };
    if !send(&front_end, started).await {
        return;
    }
    while let Some(job) = jobs.recv().await {
        let Job {
            text,
            cancel,
            done,
            timeout,
        } = job;
        {
            let mut state = lock(&shared);
            state.waiting.pop_front();
            // Stopped while it waited: it never ran, so nothing to show.
            if cancel.is_cancelled() || closed.is_cancelled() {
                drop(state);
                let done = told(done, &closed);
                settle(done, &agent, &description, id, Err(Error::Interrupted));
                continue;
            }
            state.running = Some(cancel.clone());
            state.state = Some(State::Running);
        }
        if !send(
            &front_end,
            SubagentEvent::Prompted {
                id,
                text: text.clone(),
            },
        )
        .await
        {
            return;
        }
        let began = Instant::now();
        let spends = session.usage.spends().len();
        let (tx, mut rx) = mpsc::channel::<Event>(256);
        let mut heard = true;
        let mut timed_out = false;
        // Nobody at the tab answers questions or looks at panels, and a
        // monitor's notices would go to the parent's model: headless.
        let front = FrontEnd::default();
        let result = {
            let turn = session.prompt(text, provider.as_ref(), &tools, &front, &tx, &cancel);
            tokio::pin!(turn);
            let deadline = budget(timeout);
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    result = &mut turn => break result,
                    Some(event) = rx.recv() => {
                        heard &= send(&front_end, SubagentEvent::Session { id, event }).await;
                    }
                    // Cancelling is cooperative: the turn still ends on its
                    // own, with every call answered, and the session stays
                    // valid to continue from.
                    _ = &mut deadline, if !timed_out => {
                        timed_out = true;
                        cancel.cancel();
                    }
                }
            }
        };
        // Its own cancel is what ran out, not a stop: the parent hears why.
        let result = match (result, timeout) {
            (Err(Error::Interrupted), Some(timeout)) if timed_out => Err(Error::TimedOut(timeout)),
            (result, _) => result,
        };
        // Sent just before the turn returned, and they belong before the
        // footer.
        while let Ok(event) = rx.try_recv() {
            heard &= send(&front_end, SubagentEvent::Session { id, event }).await;
        }
        // A child that was forgotten spent for a session that was left.
        if !closed.is_cancelled() {
            let turn = session.usage.spends()[spends..].iter().cloned();
            let turn = turn.map(|spend| Spend {
                agent: Some(agent.clone()),
                ..spend
            });
            lock_spent(&spent).extend(turn);
        }
        let outcome = match &result {
            Ok(()) => TaskOutcome::Completed(answer(&session)),
            Err(Error::Interrupted) => TaskOutcome::Interrupted,
            Err(e) => TaskOutcome::Failed(e.to_string()),
        };
        // Before the turn counts as ended, so whoever looks for the answer
        // once it has, on `TurnEnded` or when nothing runs any more, finds
        // it.
        settle(
            told(done, &closed),
            &agent,
            &description,
            id,
            result.map(|()| answer(&session)),
        );
        {
            let mut state = lock(&shared);
            state.running = None;
            state.state = Some(match &outcome {
                TaskOutcome::Completed(_) => State::Idle,
                TaskOutcome::Interrupted => State::Interrupted,
                TaskOutcome::Failed(_) => State::Failed,
            });
        }
        let ended = SubagentEvent::TurnEnded {
            id,
            outcome,
            elapsed: began.elapsed(),
            model: session.model.clone(),
        };
        heard &= send(&front_end, ended).await;
        if !heard {
            return;
        }
    }
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
fn answer(session: &Session) -> String {
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

/// Who still hears how a job ended: once the child is forgotten, its
/// session was left, and the model of the one after it never asked.
fn told(done: Done, closed: &CancellationToken) -> Done {
    match done {
        Done::Notify(_) if closed.is_cancelled() => Done::Nothing,
        done => done,
    }
}

/// Tells whoever waits on the job how it ended.
fn settle(done: Done, agent: &str, description: &str, id: SubagentId, turn: super::Turn) {
    match done {
        // A dropped receiver is a parent that stopped waiting, which is fine.
        Done::Reply(reply) => {
            let _ = reply.send(turn);
        }
        Done::Notify(inbox) => {
            let outcome = match turn {
                Ok(text) => TaskOutcome::Completed(text),
                Err(Error::Interrupted) => TaskOutcome::Interrupted,
                Err(e) => TaskOutcome::Failed(e.to_string()),
            };
            inbox.post_task(TaskNotice {
                id,
                agent: agent.to_string(),
                description: description.to_string(),
                outcome,
            });
        }
        Done::Nothing => {}
    }
}

/// `false` once the front-end is gone, when the actor should end: nobody
/// is left to hear from it. Without a front-end there is nothing to send.
async fn send(front_end: &Option<mpsc::Sender<SubagentEvent>>, event: SubagentEvent) -> bool {
    match front_end {
        Some(front_end) => front_end.send(event).await.is_ok(),
        None => true,
    }
}

fn lock_spent(spent: &Mutex<Vec<Spend>>) -> std::sync::MutexGuard<'_, Vec<Spend>> {
    spent.lock().expect("subagent spend lock poisoned")
}

fn lock(shared: &Mutex<ChildState>) -> std::sync::MutexGuard<'_, ChildState> {
    shared.lock().expect("subagent state lock poisoned")
}
