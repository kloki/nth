//! The task that owns a subagent's session and runs its prompts one after
//! another, reporting to the front-end and settling each job's `Done`.

use std::{
    sync::{Arc, Mutex},
    time::Instant,
};

use nth_protocol::{Event, FrontEnd, Message, Provider, TaskNotice, TaskOutcome, Tool};
use tokio::sync::mpsc;

use super::{ChildState, Done, Job, State, SubagentEvent, SubagentId};
use crate::{Error, Session};

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
    pub jobs: mpsc::UnboundedReceiver<Job>,
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
        mut jobs,
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
        let Job { text, cancel, done } = job;
        {
            let mut state = lock(&shared);
            state.queued = state.queued.saturating_sub(1);
            // Stopped while it waited: it never ran, so nothing to show.
            if cancel.is_cancelled() {
                drop(state);
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
        let (tx, mut rx) = mpsc::channel::<Event>(256);
        let mut heard = true;
        // Nobody at the tab answers questions or looks at panels, and a
        // monitor's notices would go to the parent's model: headless.
        let front = FrontEnd::default();
        let result = {
            let turn = session.prompt(text, provider.as_ref(), &tools, &front, &tx, &cancel);
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    result = &mut turn => break result,
                    Some(event) = rx.recv() => {
                        heard &= send(&front_end, SubagentEvent::Session { id, event }).await;
                    }
                }
            }
        };
        // Sent just before the turn returned, and they belong before the
        // footer.
        while let Ok(event) = rx.try_recv() {
            heard &= send(&front_end, SubagentEvent::Session { id, event }).await;
        }
        let outcome = match &result {
            Ok(()) => TaskOutcome::Completed(answer(&session)),
            Err(Error::Interrupted) => TaskOutcome::Interrupted,
            Err(e) => TaskOutcome::Failed(e.to_string()),
        };
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
        settle(
            done,
            &agent,
            &description,
            id,
            result.map(|()| answer(&session)),
        );
        if !heard {
            return;
        }
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

fn lock(shared: &Mutex<ChildState>) -> std::sync::MutexGuard<'_, ChildState> {
    shared.lock().expect("subagent state lock poisoned")
}
