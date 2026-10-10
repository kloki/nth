//! The agent loop: one user turn, step by step, run by [`Turn`]. Streaming
//! a reply with its retries is `stream`, running its tool calls `call`,
//! the guard against a model repeating one call `doom_loop`, and handing
//! the model what was sent while it worked `steer`.

mod call;
mod doom_loop;
mod steer;
mod stream;
#[cfg(test)]
pub(crate) mod tests;

use std::{collections::HashMap, time::Duration};

pub(crate) use call::{INTERRUPTED, failed, parse_args, run_call};
use doom_loop::DOOM_LOOP_THRESHOLD;
use nth_protocol::{
    AssistantMessage, BoxError, Effort, Event, Message, Provider, Tool, ToolContext,
};
use stream::Stop;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::Spend;

/// Guards against a model that never stops calling tools.
pub const DEFAULT_MAX_STEPS: usize = 100;

/// Why calls the model made on the last allowed step anyway failed.
const MAX_STEPS_REACHED: &str = "maximum steps reached; tool not run";

/// Appended to the last allowed step's request, so the model answers instead
/// of being cut off mid-tool. opencode's `max-steps.ts`, kept verbatim,
/// including its role: an assistant message.
const MAX_STEPS_PROMPT: &str = include_str!("../prompts/loop/max_steps.md");

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Provider(BoxError),
    #[error("stopped after {0} steps without a final answer")]
    TooManySteps(usize),
    #[error("interrupted")]
    Interrupted,
    /// A reply with neither text nor tool calls: a content filter, a
    /// reasoning-only reply, or a stream that ended at once. Reported
    /// rather than passed off as an answer, which a subagent's parent
    /// would read as an empty result marked completed.
    #[error("the model answered nothing")]
    EmptyReply,
    /// A subagent's turn ran past the time its parent's task tool gave it.
    #[error("no answer after {0:?}; its task_id continues it")]
    TimedOut(Duration),
    #[error(
        "stopped: the model called {0} with the same input {DOOM_LOOP_THRESHOLD} times in a row"
    )]
    DoomLoop(String),
    /// A skill run as `/name` could not be filled in, so the turn never
    /// started.
    #[error("could not run the skill: {0}")]
    Skill(String),
}

/// Where a turn's requests go: which model at what effort, on behalf of
/// which session, and how many of them the turn may make.
#[derive(Debug, Clone, Copy)]
pub struct Route<'a> {
    pub model: &'a str,
    pub effort: Effort,
    pub session_id: &'a str,
    pub max_steps: usize,
}

/// One user turn: where its requests go, what runs its tool calls, and
/// where its messages, spend and progress land. [`Turn::run`] is the agent
/// loop, and the helpers in this module's files are its methods.
pub struct Turn<'a> {
    pub provider: &'a dyn Provider,
    pub route: Route<'a>,
    pub tools: &'a [Box<dyn Tool>],
    /// What the tool calls run with.
    pub ctx: &'a ToolContext,
    /// The history. Everything the model and tools produce is appended to it.
    pub messages: &'a mut Vec<Message>,
    /// What the requests used, counted as they report it, so a turn that
    /// fails or is interrupted still counts what it spent.
    pub spend: &'a mut Spend,
    /// Where progress goes.
    pub events: &'a mpsc::Sender<Event>,
    pub cancel: &'a CancellationToken,
}

impl Turn<'_> {
    /// Runs the turn: stream a reply, run its tool calls in parallel, feed
    /// the results back, and repeat until the model answers without tools.
    ///
    /// Cancelling `cancel` ends the turn with [`Error::Interrupted`], leaving
    /// `messages` valid to continue from: partial text is kept, and every
    /// tool call has a result.
    pub async fn run(mut self) -> Result<(), Error> {
        let specs: Vec<_> = self.tools.iter().map(|t| t.spec()).collect();
        // Looked up by name once per call; a spec is too costly to build for
        // every lookup.
        let by_name: HashMap<&str, &dyn Tool> = specs
            .iter()
            .zip(self.tools)
            .map(|(spec, tool)| (spec.name, tool.as_ref()))
            .collect();
        // Where this turn's messages start, so the doom-loop guard counts
        // only calls made since the user last spoke.
        let start = self.messages.len();
        let max_steps = self.route.max_steps;
        for step in 0..max_steps {
            let last = step + 1 == max_steps;
            let reply = match self.stream_step(last, &specs).await {
                Ok(reply) => reply,
                Err(Stop::Cancelled(partial)) => {
                    if !partial.text.is_empty() || !partial.reasoning.is_empty() {
                        self.messages.push(Message::Assistant(partial));
                    }
                    return Err(Error::Interrupted);
                }
                Err(Stop::Failed(error)) => return Err(Error::Provider(error)),
            };

            if reply.text.trim().is_empty() && reply.tool_calls.is_empty() {
                // Reasoning alone is kept, as an interrupted reply's is; an
                // assistant message with nothing in it never is.
                if !reply.reasoning.is_empty() {
                    self.messages.push(Message::Assistant(reply));
                }
                return Err(Error::EmptyReply);
            }
            let calls = reply.tool_calls.clone();
            self.messages.push(Message::Assistant(reply));
            if calls.is_empty() {
                return Ok(());
            }
            // Told not to, the model called tools anyway; they don't run, and
            // every call is answered so the session stays valid.
            if last {
                self.messages
                    .extend(calls.into_iter().map(|call| Message::ToolResult {
                        call_id: call.id,
                        content: failed(MAX_STEPS_REACHED),
                    }));
                return Err(Error::TooManySteps(max_steps));
            }
            if self.preempt(&calls).await {
                continue;
            }
            self.check(start, &calls).await?;

            // Each call answers the cancel itself, so one that finished
            // before it keeps its result and every call started also
            // finishes.
            let (ctx, events, cancel) = (self.ctx, self.events, self.cancel);
            let results = futures::future::join_all(calls.iter().map(|call| {
                let tool = by_name.get(call.name.as_str()).copied();
                run_call(tool, ctx, call, events, cancel)
            }))
            .await;
            for (call, result) in calls.into_iter().zip(results) {
                self.messages.push(Message::ToolResult {
                    call_id: call.id,
                    content: result.unwrap_or_else(|reason| failed(&reason)),
                });
            }
            if cancel.is_cancelled() {
                return Err(Error::Interrupted);
            }
            self.hand_over().await;
        }
        Err(Error::TooManySteps(max_steps))
    }
}

/// The messages for the last allowed step: the history plus a prompt telling
/// the model to stop calling tools and summarize.
fn max_steps_messages(messages: &[Message]) -> Vec<Message> {
    let mut sent = messages.to_vec();
    sent.push(Message::Assistant(AssistantMessage {
        text: MAX_STEPS_PROMPT.to_string(),
        ..AssistantMessage::default()
    }));
    sent
}

async fn emit(events: &mpsc::Sender<Event>, event: Event) {
    // No listener is fine: the turn still completes and lands in `messages`.
    let _ = events.send(event).await;
}
