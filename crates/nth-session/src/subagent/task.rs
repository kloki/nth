//! The task tool: starts a subagent on a prompt, or continues one by its
//! `task_id`. With a front-end the call returns at once and the answer
//! comes back as a notice; headless it waits and returns the answer.

use std::sync::Arc;

use futures::{FutureExt, future::BoxFuture};
use nth_context::Agent;
use nth_protocol::{
    Mode, Provider, TaskNotice, TaskOutcome, Tool, ToolContext, ToolResult, ToolSpec, Writable,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{Done, Job, SubagentId, Subagents};
use crate::{Error, Session};

/// Tools a subagent never gets: another task would nest without end, and
/// the rest need the front-end, which belongs to the parent.
const WITHHELD: [&str; 5] = ["task", "question", "panel", "monitor", "monitor_stop"];
/// Tools that change files, kept from a subagent while its parent plans.
const WRITERS: [&str; 3] = ["write", "edit", "apply_patch"];

pub struct Task {
    provider: Arc<dyn Provider>,
    /// Every tool the model has; each subagent gets its share of them.
    tools: Vec<Arc<dyn Tool>>,
    subagents: Subagents,
    /// Steps a subagent's turn may take, the same as its parent's.
    max_steps: usize,
}

#[derive(Deserialize)]
struct Args {
    description: String,
    prompt: String,
    subagent_type: String,
    task_id: Option<TaskIdArg>,
}

/// Models send the id back as they read it, a number or a string.
#[derive(Deserialize)]
#[serde(untagged)]
enum TaskIdArg {
    Number(SubagentId),
    Text(String),
}

impl TaskIdArg {
    fn parse(&self) -> Result<SubagentId, String> {
        match self {
            TaskIdArg::Number(id) => Ok(*id),
            TaskIdArg::Text(text) => text
                .trim()
                .parse()
                .map_err(|_| format!("task_id {text:?} is not a number")),
        }
    }
}

impl Task {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Vec<Arc<dyn Tool>>,
        subagents: Subagents,
        max_steps: usize,
    ) -> Self {
        Self {
            provider,
            tools,
            subagents,
            max_steps,
        }
    }

    /// The tools `agent` gets: never the withheld ones, no writers while
    /// the parent may only write its plan, and only the agent's own list
    /// when it has one.
    fn tools_for(&self, agent: &Agent, writable: &Writable) -> Vec<Box<dyn Tool>> {
        let planning = matches!(writable, Writable::Only(_));
        self.tools
            .iter()
            .filter(|tool| {
                let name = tool.spec().name;
                !WITHHELD.contains(&name)
                    && !(planning && WRITERS.contains(&name))
                    && agent.allows(name)
            })
            .map(|tool| Box::new(tool.clone()) as Box<dyn Tool>)
            .collect()
    }

    /// A new subagent of `agent` in the parent's directory, on the agent's
    /// model or the parent's, in act mode: a planning parent's child is
    /// kept from writing by its tools instead, so it gets no plan file and
    /// no plan-mode reminder.
    fn spawn(&self, agent: &Agent, description: &str, ctx: &ToolContext) -> SubagentId {
        let model = agent.model.clone().unwrap_or_else(|| ctx.llm.model.clone());
        let mut session = Session::new(model, ctx.cwd.clone())
            .with_context(ctx.context.clone())
            .as_subagent(agent.prompt.clone());
        session.effort = ctx.llm.effort;
        session.mode = Mode::Act;
        session.max_steps = self.max_steps;
        let tools = self.tools_for(agent, &ctx.writable);
        self.subagents
            .spawn(agent, description, session, self.provider.clone(), tools)
    }
}

impl Tool for Task {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task",
            description: include_str!("task.md").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "description": { "type": "string", "description": "A short (3-5 words) description of the task" },
                    "prompt": { "type": "string", "description": "The task for the agent to perform" },
                    "subagent_type": { "type": "string", "description": "The type of specialized agent to use for this task" },
                    "task_id": { "type": "integer", "description": "This should only be set if you mean to resume a previous task (you can pass a prior task_id and the task will continue the same subagent session as before instead of creating a fresh one)" }
                },
                "required": ["description", "prompt", "subagent_type"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            let args: Args = crate::agent_loop::parse_args(args)?;
            if ctx.llm.model.is_empty() {
                return Err("this session has no model to run a subagent on".into());
            }
            let agents = &ctx.context.agents;
            let Some(agent) = agents.get(&args.subagent_type) else {
                let names: Vec<_> = agents.iter().map(|a| a.name.as_str()).collect();
                return Err(format!(
                    "Agent \"{}\" not found. Available agents: {}",
                    args.subagent_type,
                    names.join(", ")
                ));
            };
            let (id, started) = match &args.task_id {
                Some(id) => {
                    let id = id.parse()?;
                    if self.subagents.describe(id).is_none() {
                        return Err(format!(
                            "no subagent with task_id {id}; leave task_id out to start a new one"
                        ));
                    }
                    (id, false)
                }
                None => (self.spawn(agent, &args.description, ctx), true),
            };
            let cancel = CancellationToken::new();
            // With a front-end the answer wakes the model as a notice. Headless,
            // nothing would, so the call waits for it.
            if ctx.monitors.reaches_front_end() {
                let job = Job {
                    text: args.prompt,
                    cancel,
                    done: Done::Notify(ctx.monitors.clone()),
                };
                self.subagents.prompt(id, job);
                let verb = match started {
                    true => format!("Started subagent {id} (@{}, \"{}\")", agent.name, args.description),
                    false => format!("Sent the prompt to subagent {id} (@{})", agent.name),
                };
                return Ok(format!(
                    "{verb}. Its answer reaches you as a notice when it is done; carry on with other work or answer meanwhile. Continue it later with task_id {id}."
                ));
            }
            let (reply, waiting) = oneshot::channel();
            // Dropped with this future when the parent's turn is cancelled,
            // which stops the subagent too rather than leave it running for
            // nobody.
            let _guard = cancel.clone().drop_guard();
            let job = Job {
                text: args.prompt,
                cancel,
                done: Done::Reply(reply),
            };
            self.subagents.prompt(id, job);
            match waiting.await {
                Ok(Ok(text)) => Ok(TaskNotice {
                    id,
                    agent: agent.name.clone(),
                    description: args.description,
                    outcome: TaskOutcome::Completed(text),
                }
                .render()),
                Ok(Err(Error::Interrupted)) => Err("Task cancelled".into()),
                Ok(Err(e)) => Err(format!("Subagent failed (task_id: {id}): {e}")),
                Err(_) => Err(format!("Subagent ended unexpectedly (task_id: {id})")),
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use nth_context::{Context, Paths};
    use nth_protocol::{Llm, MonitorEvent, Monitors, StreamEvent, ToolSpec};
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        agent_loop::tests::Scripted,
        subagent::{SubagentEvent, tests::says},
    };

    /// A tool that is only a name.
    struct Named(&'static str);

    impl Tool for Named {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0,
                description: String::new(),
                parameters: json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            _: serde_json::Value,
            _: &'a ToolContext,
        ) -> BoxFuture<'a, ToolResult> {
            async { Ok(String::new()) }.boxed()
        }
    }

    fn named(names: &[&'static str]) -> Vec<Arc<dyn Tool>> {
        names
            .iter()
            .map(|n| Arc::new(Named(n)) as Arc<dyn Tool>)
            .collect()
    }

    fn context() -> Arc<Context> {
        Arc::new(Context::discover(
            std::path::Path::new("/nowhere"),
            &Paths::default(),
        ))
    }

    fn ctx() -> ToolContext {
        ToolContext {
            context: context(),
            llm: Llm {
                model: "glm-5.3".into(),
                ..Llm::default()
            },
            ..ToolContext::new("/repo".into())
        }
    }

    fn task(replies: Vec<Vec<StreamEvent>>, subagents: Subagents) -> Task {
        Task::new(
            Arc::new(Scripted::new(replies)),
            named(&["read", "write"]),
            subagents,
            10,
        )
    }

    fn names(tools: &[Box<dyn Tool>]) -> Vec<&'static str> {
        tools.iter().map(|t| t.spec().name).collect()
    }

    #[test]
    fn a_subagent_gets_its_share_of_the_tools() {
        let all = [
            "read",
            "write",
            "edit",
            "apply_patch",
            "grep",
            "task",
            "question",
            "panel",
            "monitor",
            "monitor_stop",
        ];
        let task = Task::new(
            Arc::new(Scripted::new(vec![])),
            named(&all),
            Subagents::default(),
            10,
        );
        let agents = context();
        let general = agents.agents.get("general").unwrap();
        let explore = agents.agents.get("explore").unwrap();

        assert_eq!(
            names(&task.tools_for(general, &Writable::Any)),
            ["read", "write", "edit", "apply_patch", "grep"]
        );
        assert_eq!(
            names(&task.tools_for(general, &Writable::Only("/repo/.nth/plans/x.md".into()))),
            ["read", "grep"],
            "a planning parent's child may not write"
        );
        assert_eq!(
            names(&task.tools_for(explore, &Writable::Any)),
            ["read", "grep"]
        );
    }

    #[tokio::test]
    async fn headless_it_waits_and_returns_the_answer_as_a_task_element() {
        let subagents = Subagents::default();
        let task = task(
            vec![
                says("Tabs open in content.rs."),
                says("And close there too."),
            ],
            subagents.clone(),
        );
        let args =
            json!({ "description": "find tabs", "prompt": "where?", "subagent_type": "explore" });

        let result = task.call(args, &ctx()).await.unwrap();

        assert_eq!(
            result,
            "<task id=\"1\" agent=\"explore\" description=\"find tabs\" state=\"completed\">\n\
             <task_result>\nTabs open in content.rs.\n</task_result>\n</task>"
        );
        let again = json!({ "description": "find tabs", "prompt": "and closing?", "subagent_type": "explore", "task_id": "1" });
        let result = task.call(again, &ctx()).await.unwrap();
        assert!(
            result.starts_with("<task id=\"1\" "),
            "the same subagent: {result}"
        );
        assert!(result.contains("And close there too."), "{result}");
        assert_eq!(subagents.ids(), [1]);
    }

    #[tokio::test]
    async fn unknown_agents_ids_and_a_missing_model_are_errors() {
        let task = task(vec![], Subagents::default());

        let unknown = json!({ "description": "d", "prompt": "p", "subagent_type": "nobody" });
        assert_eq!(
            task.call(unknown, &ctx()).await.unwrap_err(),
            "Agent \"nobody\" not found. Available agents: explore, general"
        );
        let stale =
            json!({ "description": "d", "prompt": "p", "subagent_type": "explore", "task_id": 7 });
        assert_eq!(
            task.call(stale, &ctx()).await.unwrap_err(),
            "no subagent with task_id 7; leave task_id out to start a new one"
        );
        let fine = json!({ "description": "d", "prompt": "p", "subagent_type": "explore" });
        assert_eq!(
            task.call(fine, &ToolContext::new("/repo".into()))
                .await
                .unwrap_err(),
            "this session has no model to run a subagent on"
        );
    }

    #[tokio::test]
    async fn with_a_front_end_it_returns_at_once_and_the_answer_becomes_a_notice() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let (monitor_tx, _monitor_rx) = mpsc::channel::<MonitorEvent>(4);
        let inbox = Monitors::new(monitor_tx, "/logs".into());
        let task = task(vec![says("found")], subagents);
        let ctx = ToolContext {
            monitors: inbox.clone(),
            ..ctx()
        };
        let args =
            json!({ "description": "find tabs", "prompt": "where?", "subagent_type": "explore" });

        let result = task.call(args, &ctx).await.unwrap();

        assert_eq!(
            result,
            "Started subagent 1 (@explore, \"find tabs\"). Its answer reaches you as a notice when it is done; \
             carry on with other work or answer meanwhile. Continue it later with task_id 1."
        );
        while let Some(event) = rx.recv().await {
            if matches!(event, SubagentEvent::TurnEnded { .. }) {
                break;
            }
        }
        assert!(inbox.has_notices());
        assert!(
            inbox
                .take_notices()
                .unwrap()
                .contains("<task_result>\nfound\n</task_result>")
        );
    }
}
