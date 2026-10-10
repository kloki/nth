//! The task tool: starts a subagent on a prompt, or continues one by its
//! `task_id`. With a front-end the call returns at once and the answer
//! comes back as a notice; headless it waits and returns the answer.

use std::{sync::Arc, time::Duration};

use futures::{FutureExt, future::BoxFuture};
use nth_context::Agent;
use nth_protocol::{
    Mode, Provider, TaskNotice, TaskOutcome, Tool, ToolContext, ToolResult, ToolSpec, Writable,
};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use super::{Done, Isolation, Job, SubagentId, Subagents, WRITERS};
use crate::{Error, Session};

/// Tools a subagent never gets: another task would nest without end, the
/// worktree tools would take it out of its parent's directory, and the rest
/// need the front-end, which belongs to the parent.
const WITHHELD: [&str; 8] = [
    "task",
    "task_stop",
    "question",
    "panel",
    "monitor",
    "monitor_stop",
    "enter_worktree",
    "exit_worktree",
];
/// Goes with every prompt from a planning parent: its child has no tool
/// that writes, but bash could still change files.
const PLANNING: &str = "<system-reminder>\nThe agent that sent you this task is planning and may not change the project. Do not change any file, with bash or otherwise: research, then report what you found.\n</system-reminder>";

pub struct Task {
    provider: Arc<dyn Provider>,
    /// Every tool the model has; each subagent gets its share of them.
    tools: Vec<Arc<dyn Tool>>,
    subagents: Subagents,
    limits: Limits,
}

/// What one task may spend before it fails: a child works on one
/// delegated question, and a parent waiting on it should hear back.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Steps a subagent's turn may take.
    pub max_steps: usize,
    /// How long a turn the model started may run; `None` leaves it to its
    /// steps.
    pub timeout: Option<Duration>,
}

#[derive(Deserialize)]
struct Args {
    description: String,
    prompt: String,
    subagent_type: String,
    task_id: Option<TaskIdArg>,
    isolation: Option<IsolationArg>,
}

/// Where a new subagent works; only a worktree of its own for now.
#[derive(Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum IsolationArg {
    Worktree,
}

/// Models send the id back as they read it, a number or a string.
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum TaskIdArg {
    Number(SubagentId),
    Text(String),
}

impl TaskIdArg {
    pub(super) fn parse(&self) -> Result<SubagentId, String> {
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
        limits: Limits,
    ) -> Self {
        Self {
            provider,
            tools,
            subagents,
            limits,
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

    /// A new subagent of `agent` in the parent's directory, or in a
    /// worktree of its own when `worktree`, on the agent's model or the
    /// parent's, in act mode: a planning parent's child is kept from
    /// writing by its tools instead, so it gets no plan file and no
    /// plan-mode reminder.
    async fn spawn(
        &self,
        agent: &Agent,
        description: &str,
        worktree: bool,
        ctx: &ToolContext,
    ) -> Result<SubagentId, String> {
        let isolation = match worktree {
            // Worktrees go in the main checkout, wherever the parent is.
            true => {
                let origin = ctx.workdir.origin().unwrap_or_else(|| ctx.cwd.clone());
                Some(Isolation::new(&origin, description).await?)
            }
            false => None,
        };
        let model = agent.model.clone().unwrap_or_else(|| ctx.llm.model.clone());
        let mut session = Session::new(model, ctx.cwd.clone())
            .with_extra_dirs(ctx.extra_dirs.clone())
            .with_context(ctx.context.clone())
            .as_subagent(agent.prompt.clone());
        session.effort = ctx.llm.effort;
        session.mode = Mode::Act;
        session.max_steps = self.limits.max_steps;
        let tools = self.tools_for(agent, &ctx.writable);
        Ok(self.subagents.spawn_in(
            agent,
            description,
            session,
            self.provider.clone(),
            tools,
            isolation,
        ))
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
                    "task_id": { "type": "integer", "description": "This should only be set if you mean to resume a previous task (you can pass a prior task_id and the task will continue the same subagent session as before instead of creating a fresh one)" },
                    "isolation": { "type": "string", "enum": ["worktree"], "description": "\"worktree\" runs a new agent in a git worktree of its own, so the files it changes stay out of this checkout" }
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
            let planning = matches!(ctx.writable, Writable::Only(_));
            let (id, started) = match &args.task_id {
                Some(id) => {
                    let id = id.parse()?;
                    match self.subagents.describe(id) {
                        None => {
                            return Err(format!(
                                "no subagent with task_id {id}; leave task_id out to start a new one"
                            ));
                        }
                        Some((other, _)) if other != agent.name => {
                            return Err(format!(
                                "subagent {id} is @{other}, not @{}; leave task_id out to start a new one",
                                agent.name
                            ));
                        }
                        Some(_) => {}
                    }
                    // Its tools were picked when it started, outside plan mode.
                    if planning && self.subagents.writes(id) {
                        return Err(format!(
                            "subagent {id} can change files, which is not allowed while planning; leave task_id out to start a read-only one"
                        ));
                    }
                    (id, false)
                }
                None => {
                    let worktree =
                        args.isolation == Some(IsolationArg::Worktree) || agent.worktree;
                    if worktree && planning {
                        return Err("a subagent in a worktree is for changing files, which is not allowed while planning; leave isolation out".into());
                    }
                    let id = self.spawn(agent, &args.description, worktree, ctx).await?;
                    (id, true)
                }
            };
            let text = match planning {
                true => format!("{}\n\n{PLANNING}", args.prompt),
                false => args.prompt,
            };
            let cancel = CancellationToken::new();
            // With an inbox the answer wakes the model as a notice. Headless
            // there is none, so the call waits for it.
            if ctx.inbox.reaches_model() {
                let job = Job {
                    text,
                    cancel,
                    done: Done::Notify(ctx.inbox.clone()),
                    timeout: self.limits.timeout,
                };
                if !self.subagents.prompt(id, job) {
                    return Err(format!("subagent {id} has ended; leave task_id out to start a new one"));
                }
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
                text,
                cancel,
                done: Done::Reply(reply),
                timeout: self.limits.timeout,
            };
            if !self.subagents.prompt(id, job) {
                return Err(format!("subagent {id} has ended; leave task_id out to start a new one"));
            }
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
    use nth_protocol::{Inbox, Llm, StreamEvent, ToolSpec};
    use tokio::sync::mpsc;

    use super::*;
    use crate::{
        agent_loop::tests::Scripted,
        subagent::{
            SubagentEvent,
            tests::{Stalled, says},
        },
    };

    /// Ten steps and no clock, as the tests here need.
    const LIMITS: Limits = Limits {
        max_steps: 10,
        timeout: None,
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
            LIMITS,
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
            "task_stop",
            "question",
            "panel",
            "monitor",
            "monitor_stop",
        ];
        let task = Task::new(
            Arc::new(Scripted::new(vec![])),
            named(&all),
            Subagents::default(),
            LIMITS,
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
    async fn a_subagent_works_in_the_directories_added_to_its_parent() {
        let provider = Arc::new(Scripted::new(vec![says("done")]));
        let task = Task::new(provider.clone(), Vec::new(), Subagents::default(), LIMITS);
        let ctx = ToolContext {
            extra_dirs: vec!["/elsewhere".into()],
            ..ctx()
        };
        let args = json!({ "description": "d", "prompt": "p", "subagent_type": "explore" });

        task.call(args, &ctx).await.unwrap();

        let systems = provider.systems.lock().expect("not poisoned");
        assert!(systems[0].contains("  - /elsewhere\n"), "{}", systems[0]);
    }

    #[tokio::test]
    async fn headless_a_task_past_its_budget_fails_and_says_so() {
        let task = Task::new(
            Arc::new(Stalled),
            Vec::new(),
            Subagents::default(),
            Limits {
                max_steps: 10,
                timeout: Some(Duration::from_millis(20)),
            },
        );
        let args = json!({ "description": "d", "prompt": "p", "subagent_type": "explore" });

        assert_eq!(
            task.call(args, &ctx()).await.unwrap_err(),
            "Subagent failed (task_id: 1): no answer after 20ms; its task_id continues it"
        );
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
    async fn with_an_inbox_it_returns_at_once_and_the_answer_becomes_a_notice() {
        let (tx, mut rx) = mpsc::channel(64);
        let subagents = Subagents::new(tx);
        let inbox = Inbox::new();
        let task = task(vec![says("found")], subagents);
        let ctx = ToolContext {
            inbox: inbox.clone(),
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

    #[tokio::test]
    async fn a_planning_parent_continues_no_writer_and_no_other_agent() {
        let subagents = Subagents::default();
        let task = task(
            vec![says("one"), says("two"), says("three")],
            subagents.clone(),
        );
        let start =
            |agent: &str| json!({ "description": "d", "prompt": "p", "subagent_type": agent });
        let again = |agent: &str| json!({ "description": "d", "prompt": "p", "subagent_type": agent, "task_id": 1 });
        let planning = ToolContext {
            writable: Writable::Only("/repo/.nth/plans/x.md".into()),
            ..ctx()
        };

        task.call(start("general"), &ctx()).await.unwrap();

        assert_eq!(
            task.call(again("explore"), &ctx()).await.unwrap_err(),
            "subagent 1 is @general, not @explore; leave task_id out to start a new one"
        );
        assert_eq!(
            task.call(again("general"), &planning).await.unwrap_err(),
            "subagent 1 can change files, which is not allowed while planning; leave task_id out to start a read-only one"
        );
        task.call(start("general"), &planning).await.unwrap();
        assert!(!subagents.writes(2), "started while planning");
        let reused =
            json!({ "description": "d", "prompt": "p", "subagent_type": "general", "task_id": 2 });
        assert!(task.call(reused, &planning).await.is_ok());
    }

    /// Says where it runs, and makes a file there when asked to.
    #[derive(Default)]
    struct Make(std::sync::Mutex<Vec<std::path::PathBuf>>);

    impl Tool for Make {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "make",
                description: String::new(),
                parameters: json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            args: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> BoxFuture<'a, ToolResult> {
            self.0.lock().expect("not poisoned").push(ctx.cwd.clone());
            if args == json!({ "file": true }) {
                std::fs::write(ctx.cwd.join("made.txt"), "made").expect("write");
            }
            async { Ok(String::new()) }.boxed()
        }
    }

    /// A task for @general in a worktree of `root`, whose child calls make
    /// with `args` and then answers.
    async fn in_worktree(root: &std::path::Path, args: &str) -> (Arc<Make>, ToolResult) {
        use crate::agent_loop::tests::call;

        let make = Arc::new(Make::default());
        let task = Task::new(
            Arc::new(Scripted::new(vec![
                vec![StreamEvent::ToolCall(call("1", "make", args))],
                says("done"),
            ])),
            vec![make.clone()],
            Subagents::default(),
            LIMITS,
        );
        let args = json!({ "description": "Make a file", "prompt": "p", "subagent_type": "general", "isolation": "worktree" });
        let ctx = ToolContext {
            cwd: root.to_path_buf(),
            ..ctx()
        };
        let result = task.call(args, &ctx).await;
        (make, result)
    }

    #[tokio::test]
    async fn a_worktree_that_changed_nothing_is_gone_after_the_answer() {
        let (_dir, root) = crate::subagent::worktree::tests::repo();

        let (make, result) = in_worktree(&root, "{}").await;

        let worktree = root.join(".nth/worktrees/make-a-file");
        assert_eq!(*make.0.lock().unwrap(), std::slice::from_ref(&worktree));
        let result = result.unwrap();
        assert!(!result.contains("worktree"), "{result}");
        assert!(!worktree.exists());
    }

    #[tokio::test]
    async fn a_worktree_with_changes_is_kept_and_named_in_the_answer() {
        let (_dir, root) = crate::subagent::worktree::tests::repo();

        let (_, result) = in_worktree(&root, r#"{"file":true}"#).await;

        let worktree = root.join(".nth/worktrees/make-a-file");
        let result = result.unwrap();
        assert!(
            result.contains(&format!(
                "done\n\nWorked in the worktree {} on branch make-a-file, which has uncommitted changes.",
                worktree.display()
            )),
            "{result}"
        );
        assert!(worktree.join("made.txt").exists());
        assert!(
            !root.join("made.txt").exists(),
            "the parent's checkout is untouched"
        );
    }

    #[tokio::test]
    async fn a_planning_parent_gets_no_worktree() {
        let task = task(vec![], Subagents::default());
        let args = json!({ "description": "d", "prompt": "p", "subagent_type": "general", "isolation": "worktree" });
        let planning = ToolContext {
            writable: Writable::Only("/repo/.nth/plans/x.md".into()),
            ..ctx()
        };

        let out = task.call(args, &planning).await;

        assert!(out.unwrap_err().contains("not allowed while planning"));
    }
}
