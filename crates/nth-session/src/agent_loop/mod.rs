//! The agent loop: one user turn, step by step, run by [`Turn`]. Streaming
//! a reply with its retries is `stream`, running its tool calls `call`
//! (their arguments mended against the tool's schema by `repair`), the
//! guard against a model repeating one call `doom_loop`, and handing
//! the model what was sent while it worked `steer`.

mod call;
mod doom_loop;
mod repair;
mod steer;
mod stream;
#[cfg(test)]
pub(crate) mod tests;

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

pub(crate) use call::run_call;
use doom_loop::DOOM_LOOP_THRESHOLD;
use nth_protocol::{
    AssistantMessage, BoxError, ContextOverflow, Effort, Event, Message, Provider, Request, Tool,
    ToolContext, ToolSpec, Usage, failed,
};
pub(crate) use stream::Stop;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{Compaction, Spend};

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
    /// The conversation cannot be shrunk below the window: the summary
    /// request was refused as too long even with its oldest exchanges
    /// dropped, a step overflowed again right after a compaction, or
    /// compacting changed nothing while the session was still over.
    #[error("cannot compact: the conversation cannot be shrunk below the context window")]
    TooBigToCompact,
}

/// Where a turn's requests go: which model at what effort, on behalf of
/// which session, how many of them the turn may make, and how it keeps
/// itself inside the model's window.
#[derive(Debug, Clone, Copy)]
pub struct Route<'a> {
    pub model: &'a str,
    pub effort: Effort,
    pub session_id: &'a str,
    /// The session that started this one, for a subagent.
    pub parent_session_id: Option<&'a str>,
    pub max_steps: usize,
    /// The model's context window, when the front-end knows it; without it
    /// nothing compacts on its own.
    pub context_window: Option<u64>,
    /// The model's output limit, for the reserved buffer's default.
    pub max_output: Option<u64>,
    /// How the session compacts, from its own config.
    pub compaction: Compaction,
}

impl<'a> Route<'a> {
    /// A request on this route carrying `messages` and offering `tools`.
    pub(crate) fn request(&self, messages: &'a [Message], tools: &'a [ToolSpec]) -> Request<'a> {
        Request {
            model: self.model,
            session_id: self.session_id,
            parent_session_id: self.parent_session_id,
            effort: self.effort,
            messages,
            tools,
        }
    }
}

/// One user turn: where its requests go, what runs its tool calls, and
/// where its messages, spend and progress land. [`Turn::run`] is the agent
/// loop, and the helpers in this module's files and in compaction's
/// `turn.rs` are its methods.
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
    /// What the last finished reply reported it used, the automatic
    /// compaction's trigger: seeded from the session's own, so a resumed
    /// session compacting before its first request knows how full the
    /// window was, and written back on every exit. It is one reply's
    /// usage, not the turn's sum, whose input counts every step's whole
    /// history over again.
    pub last_usage: &'a mut Option<Usage>,
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
    /// tool call has a result. The last reply's usage is written back
    /// whatever the outcome, so even a failed or interrupted turn leaves
    /// the session knowing how full its window was.
    pub async fn run(mut self) -> Result<(), Error> {
        let mut last_usage = *self.last_usage;
        let result = self.step_loop(&mut last_usage).await;
        *self.last_usage = last_usage;
        result
    }

    /// The step loop [`Turn::run`] wraps, with the usage the trigger reads.
    async fn step_loop(&mut self, last_usage: &mut Option<Usage>) -> Result<(), Error> {
        self.hand_over_curios().await;
        // A primary agent is offered only the tools it allows.
        let (specs, tools): (Vec<_>, Vec<_>) = self
            .tools
            .iter()
            .map(|t| (t.spec(), t.as_ref()))
            .filter(|(spec, _)| {
                !(self.ctx.is_subagent && nth_protocol::tool_names::CURIOS.contains(&spec.name))
                    && self
                        .ctx
                        .agent
                        .as_ref()
                        .is_none_or(|agent| agent.allows(spec.name))
            })
            .unzip();
        // Looked up by name once per call; a spec is too costly to build for
        // every lookup.
        let by_name: HashMap<&str, &dyn Tool> = specs
            .iter()
            .zip(tools)
            .map(|(spec, tool)| (spec.name, tool))
            .collect();
        // Where this turn's messages start, so the doom-loop guard counts
        // only calls made since the user last spoke.
        let start = self.messages.len();
        let max_steps = self.route.max_steps;
        // A step is one request the model answers; compaction is
        // maintenance for the next one, so it does not use a step and the
        // last allowed step still gets its request.
        let mut step = 0;
        // One overflow per step is worth retrying under a summary; the
        // same step overflowing again will not shrink. A later step may
        // overflow afresh, as opencode's per-step recovery allows.
        let mut replayed = false;
        while step < max_steps {
            // A conversation that reached the usable window is compacted
            // before another request is sent — including one a resumed
            // session left over it — and one compacting cannot get under
            // the window stops the turn rather than summarizing every step.
            if self.compaction_due(*last_usage) {
                if !self.compact().await? {
                    return Err(Error::TooBigToCompact);
                }
                *last_usage = None;
                if self.still_over_the_window() {
                    return Err(Error::TooBigToCompact);
                }
                continue;
            }
            let last = step + 1 == max_steps;
            let reply = match self.stream_step(last, &specs, last_usage).await {
                Ok(reply) => reply,
                Err(Stop::Cancelled(partial)) => {
                    if !partial.text.is_empty() || !partial.reasoning.is_empty() {
                        self.messages.push(Message::Assistant(partial));
                    }
                    return Err(Error::Interrupted);
                }
                Err(Stop::Failed(error)) => {
                    // The provider refused the conversation as too long:
                    // summarize it and send the step again from the
                    // checkpoint, which carries the latest exchange.
                    if error.downcast_ref::<ContextOverflow>().is_some() {
                        if replayed || !self.compact().await? {
                            return Err(Error::TooBigToCompact);
                        }
                        replayed = true;
                        *last_usage = None;
                        continue;
                    }
                    return Err(Error::Provider(error));
                }
            };
            step += 1;
            replayed = false;

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
                        images: Vec::new(),
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
            let answers = futures::future::join_all(calls.iter().map(|call| {
                let tool = by_name.get(call.name.as_str()).copied();
                async move {
                    let began = Instant::now();
                    let message = run_call(tool, ctx, call, events, cancel).await;
                    (call, message, began.elapsed())
                }
            }))
            .await;
            // What each call did, before the results become history.
            for (call, message, took) in &answers {
                self.spend.tool(&call.name, message, *took);
            }
            self.messages
                .extend(answers.into_iter().map(|(_, message, _)| message));
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

pub(crate) async fn emit(events: &mpsc::Sender<Event>, event: Event) {
    // No listener is fine: the turn still completes and lands in `messages`.
    let _ = events.send(event).await;
}
