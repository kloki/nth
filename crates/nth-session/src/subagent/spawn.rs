//! What the task tool and `work_assign` share to start a subagent: finding
//! the agent the model named, and starting it on its share of the tools.

use std::sync::Arc;

use nth_context::Agent;
use nth_protocol::{Provider, Tool, ToolContext, Writable};

use super::{Limits, SubagentId, Subagents, child_session, tools_for};

/// The subagent the model named, or the refusal listing the ones there
/// are.
pub(crate) fn find_agent<'a>(ctx: &'a ToolContext, name: &str) -> Result<&'a Agent, String> {
    let agents = &ctx.context.agents;
    agents.subagent(name).ok_or_else(|| {
        let names: Vec<_> = agents.subagents().map(|a| a.name.as_str()).collect();
        format!(
            "Agent \"{name}\" not found. Available agents: {}",
            names.join(", ")
        )
    })
}

/// Starts subagents for a tool: on the parent's provider, each with its
/// share of every tool the model has.
pub(crate) struct Spawner {
    provider: Arc<dyn Provider>,
    tools: Vec<Arc<dyn Tool>>,
    subagents: Subagents,
    limits: Limits,
}

impl Spawner {
    pub(crate) fn new(
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        subagents: Subagents,
        limits: Limits,
    ) -> Self {
        Self {
            provider,
            tools,
            subagents,
            limits,
        }
    }

    pub(crate) fn subagents(&self) -> &Subagents {
        &self.subagents
    }

    pub(crate) fn limits(&self) -> Limits {
        self.limits
    }

    /// The tools `agent` gets.
    pub(crate) fn tools_for(&self, agent: &Agent, writable: &Writable) -> Vec<Box<dyn Tool>> {
        tools_for(&self.tools, agent, writable)
    }

    /// Starts a subagent of `agent` for the call `ctx`, with `extra` tools
    /// beside its share, and returns its id.
    pub(crate) fn spawn(
        &self,
        agent: &Agent,
        description: &str,
        ctx: &ToolContext,
        extra: Vec<Box<dyn Tool>>,
    ) -> SubagentId {
        let session = child_session(agent, ctx, self.limits.max_steps);
        let mut tools = self.tools_for(agent, &ctx.writable);
        tools.extend(extra);
        self.subagents
            .spawn(agent, description, session, self.provider.clone(), tools)
    }

    /// Refuses to continue subagent `id` from a planning parent when it
    /// can change files: its tools were picked when it started, outside
    /// plan mode.
    pub(crate) fn read_only(&self, id: SubagentId, planning: bool) -> Result<(), String> {
        match planning && self.subagents.writes(id) {
            true => Err(format!(
                "subagent {id} can change files, which is not allowed while planning; leave task_id out to start a read-only one"
            )),
            false => Ok(()),
        }
    }
}
