//! What the task tool and `work_assign` share to start a subagent: finding
//! the agent the model named, and starting it on its share of the tools.

use std::{borrow::Cow, sync::Arc};

use nth_context::Agent;
use nth_protocol::{Provider, Tool, ToolContext, Writable};

use super::{Limits, Reserved, SubagentId, Subagents, child_session, tools_for};

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

    /// Starts a subagent of `agent` for the call `ctx` as the number
    /// `reserved` holds, with `extra` tools beside its share, and returns
    /// its id.
    pub(crate) fn spawn(
        &self,
        reserved: Reserved,
        agent: &Agent,
        description: &str,
        ctx: &ToolContext,
        extra: Vec<Box<dyn Tool>>,
    ) -> SubagentId {
        // The agent's own model is as its frontmatter wrote it, perhaps
        // bare; qualified, its tab names it as the picker would.
        let agent = match &agent.model {
            Some(model) => Cow::Owned(Agent {
                model: Some(self.provider.qualify(model)),
                ..agent.clone()
            }),
            None => Cow::Borrowed(agent),
        };
        let agent = agent.as_ref();
        let session = child_session(agent, ctx, self.limits.max_steps);
        let mut tools = self.tools_for(agent, &ctx.writable);
        tools.extend(extra);
        self.subagents.spawn(
            reserved,
            agent,
            description,
            session,
            self.provider.clone(),
            tools,
        )
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

#[cfg(test)]
mod tests {
    use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
    use nth_protocol::{BoxError, Listing, Request, StreamEvent};
    use tokio::sync::mpsc;

    use super::*;
    use crate::subagent::{SubagentEvent, tests::explore};

    /// A provider that names every model as Zen's, as `Providers` does a
    /// bare id when Zen is the default.
    struct Zen;

    impl Provider for Zen {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            async { Ok(Listing::default()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            async { Err("unused".into()) }.boxed()
        }

        fn qualify(&self, model: &str) -> String {
            format!("zen/{model}")
        }
    }

    #[tokio::test]
    async fn a_subagents_own_model_is_qualified() {
        let (tx, mut rx) = mpsc::channel(8);
        let limits = Limits {
            max_steps: 5,
            timeout: None,
            stall: None,
        };
        let spawner = Spawner::new(Arc::new(Zen), Vec::new(), Subagents::new(tx), limits);
        let mut agent = explore();
        agent.model = Some("glm".into());

        spawner.spawn(
            spawner.subagents().reserve(),
            &agent,
            "look",
            &ToolContext::new("/repo".into()),
            Vec::new(),
        );

        let Some(SubagentEvent::Started { model, .. }) = rx.recv().await else {
            panic!("a tab first");
        };
        assert_eq!(model, "zen/glm");
    }
}
