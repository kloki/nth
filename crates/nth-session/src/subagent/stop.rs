//! The task_stop tool: the model stops a subagent it no longer needs, by
//! its `task_id`, as monitor_stop stops a monitor.

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;

use super::{Subagents, task::TaskIdArg};

pub struct TaskStop {
    subagents: Subagents,
}

#[derive(Deserialize)]
struct Args {
    task_id: TaskIdArg,
}

impl TaskStop {
    pub fn new(subagents: Subagents) -> Self {
        Self { subagents }
    }
}

impl Tool for TaskStop {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task_stop",
            description: include_str!("stop.md").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "integer", "minimum": 1, "description": "The subagent's id, from the task tool's result" }
                },
                "required": ["task_id"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        _: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            let args: Args = crate::agent_loop::parse_args(args)?;
            let id = args.task_id.parse()?;
            if self.subagents.cancel(id) {
                return Ok(format!(
                    "Stopped subagent {id}. Its interrupted notice follows; task_id {id} continues it if you need it again."
                ));
            }
            match self.subagents.describe(id) {
                Some(_) => Err(format!("subagent {id} is not running")),
                None => Err(format!("no subagent with task_id {id}")),
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nth_context::{Context, Paths};
    use nth_protocol::{Inbox, Llm, TaskOutcome};
    use tokio::sync::mpsc;

    use super::*;
    use crate::subagent::{
        Limits, SubagentEvent, Task,
        tests::{Stalled, says},
    };

    fn ctx(inbox: Inbox) -> ToolContext {
        ToolContext {
            context: Arc::new(Context::discover(
                std::path::Path::new("/nowhere"),
                &Paths::default(),
            )),
            llm: Llm {
                model: "glm-5.3".into(),
                ..Llm::default()
            },
            inbox,
            ..ToolContext::new("/repo".into())
        }
    }

    const LIMITS: Limits = Limits {
        max_steps: 10,
        timeout: None,
    };

    #[tokio::test]
    async fn stopping_a_running_subagent_interrupts_it_and_the_model_hears() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let inbox = Inbox::new();
        let task = Task::new(Arc::new(Stalled), Vec::new(), subagents.clone(), LIMITS);
        let stop = TaskStop::new(subagents.clone());
        let start = json!({ "description": "d", "prompt": "p", "subagent_type": "explore" });
        task.call(start, &ctx(inbox.clone())).await.unwrap();
        // Stopping what still waits in the inbox skips it without a turn;
        // the test is about a turn under way.
        while !matches!(rx.recv().await, Some(SubagentEvent::Prompted { .. })) {}

        let result = stop
            .call(json!({ "task_id": 1 }), &ctx(inbox.clone()))
            .await;

        assert_eq!(
            result.unwrap(),
            "Stopped subagent 1. Its interrupted notice follows; task_id 1 continues it if you need it again."
        );
        loop {
            match rx.recv().await {
                Some(SubagentEvent::TurnEnded { outcome, .. }) => {
                    assert_eq!(outcome, TaskOutcome::Interrupted);
                    break;
                }
                Some(_) => {}
                None => panic!("the actor ended without a footer"),
            }
        }
        assert_eq!(
            inbox.take_notices().unwrap(),
            "<task id=\"1\" agent=\"explore\" description=\"d\" state=\"interrupted\"/>"
        );
        assert_eq!(
            stop.call(json!({ "task_id": "1" }), &ctx(inbox))
                .await
                .unwrap_err(),
            "subagent 1 is not running",
            "idle now, and the id arrives as text too"
        );
    }

    #[tokio::test]
    async fn an_idle_or_unknown_subagent_cannot_be_stopped() {
        let (tx, _rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let inbox = Inbox::new();
        let provider = Arc::new(crate::agent_loop::tests::Scripted::new(vec![says("done")]));
        let task = Task::new(provider, Vec::new(), subagents.clone(), LIMITS);
        let stop = TaskStop::new(subagents.clone());
        task.call(
            json!({ "description": "d", "prompt": "p", "subagent_type": "explore" }),
            &ctx(inbox.clone()),
        )
        .await
        .unwrap();
        while subagents.state(1) != Some(crate::subagent::State::Idle) {
            tokio::task::yield_now().await;
        }

        assert_eq!(
            stop.call(json!({ "task_id": 1 }), &ctx(inbox.clone()))
                .await
                .unwrap_err(),
            "subagent 1 is not running"
        );
        assert_eq!(
            stop.call(json!({ "task_id": 7 }), &ctx(inbox))
                .await
                .unwrap_err(),
            "no subagent with task_id 7"
        );
    }
}
