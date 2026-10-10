use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::SystemTime,
};

use nth_context::Context;
use nth_protocol::{
    AssistantMessage, Effort, Event, FrontEnd, Llm, Message, Mode, Provider, Tool, ToolCall,
    ToolContext, Workdir, Writable,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    DEFAULT_MAX_STEPS, Error, Route,
    agent_loop::{failed, run_call},
    plan::{self, Approver},
    run_turn, subagent, system_prompt,
};

fn first_line(text: &str) -> &str {
    text.trim().lines().next().unwrap_or_default()
}

/// The user message before a command the user ran themselves, as opencode
/// words it. Front-ends that replay a session leave it out.
pub const SHELL_PROMPT: &str = "The following tool was executed by the user";

/// One conversation: who it runs for, where, and everything said so far.
/// Serializable so the [`Store`](crate::Store) can persist and resume it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Also sent to the provider so it can route and cache per conversation.
    pub id: Uuid,
    pub cwd: PathBuf,
    /// The directory the session left to work in a worktree, which
    /// `exit_worktree` goes back to; `None` outside one.
    #[serde(default)]
    pub origin: Option<PathBuf>,
    /// Working directories added with `/add-dir`, which the tools may read
    /// and edit like `cwd` itself.
    #[serde(default)]
    pub extra_dirs: Vec<PathBuf>,
    pub model: String,
    /// Sessions saved before effort existed load with the model's default.
    #[serde(default)]
    pub effort: Effort,
    /// Sessions saved before modes existed load as act, which is how they
    /// ran.
    #[serde(default)]
    pub mode: Mode,
    /// The mode the last turn ran in, so the model is told when it enters
    /// plan mode or leaves it; `None` before the first turn.
    #[serde(default)]
    last_turn_mode: Option<Mode>,
    pub created_at: SystemTime,
    /// When a prompt was last sent; saved sessions are listed by it.
    pub updated_at: SystemTime,
    pub messages: Vec<Message>,
    /// Instruction files the read tool has attached, so a resumed session
    /// does not get them again.
    #[serde(default)]
    pub loaded_instructions: BTreeSet<PathBuf>,
    /// Model requests a turn may make before it gives up. Comes from the
    /// config, not the save, so a resumed session follows today's config.
    #[serde(skip, default = "default_max_steps")]
    pub max_steps: usize,
    /// Runs for another session's model rather than for you: it has no
    /// task tool, so it is told of no agents and `@name` means nothing.
    #[serde(default)]
    pub subagent: bool,
    /// A subagent's own system prompt, in place of the model's persona;
    /// `None` keeps the persona.
    #[serde(default)]
    persona: Option<String>,
    /// Not saved: it is read afresh for the working directory, so a resumed
    /// session sees the instruction files as they are now.
    #[serde(skip)]
    context: Arc<Context>,
}

impl Session {
    pub fn new(model: impl Into<String>, cwd: PathBuf) -> Self {
        let model = model.into();
        let context = Arc::<Context>::default();
        let messages = vec![Message::System(system_prompt(&model, &cwd, &[], &context))];
        let now = SystemTime::now();
        Self {
            id: Uuid::new_v4(),
            cwd,
            origin: None,
            extra_dirs: Vec::new(),
            model,
            effort: Effort::default(),
            mode: Mode::default(),
            last_turn_mode: None,
            created_at: now,
            updated_at: now,
            messages,
            loaded_instructions: BTreeSet::new(),
            max_steps: DEFAULT_MAX_STEPS,
            subagent: false,
            persona: None,
            context,
        }
    }

    /// Makes this a subagent's session, with `persona` as its system prompt
    /// in place of the model's, or the model's when `None`.
    pub fn as_subagent(mut self, persona: Option<String>) -> Self {
        self.subagent = true;
        self.persona = persona;
        self.rewrite_system_prompt();
        self
    }

    /// Builder form of [`Session::set_context`].
    pub fn with_context(mut self, context: Arc<Context>) -> Self {
        self.set_context(context);
        self
    }

    /// Puts `context` into the system prompt. A loaded session has none
    /// until this is called.
    pub fn set_context(&mut self, context: Arc<Context>) {
        self.context = context;
        self.rewrite_system_prompt();
    }

    /// Builder form of [`Session::set_extra_dirs`].
    pub fn with_extra_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.set_extra_dirs(dirs);
        self
    }

    /// The directories added with `/add-dir` become part of the system
    /// prompt, as a switch of model does.
    pub fn set_extra_dirs(&mut self, dirs: Vec<PathBuf>) {
        self.extra_dirs = dirs;
        self.rewrite_system_prompt();
    }

    /// Moves the session to `cwd`, which the system prompt names. `origin`
    /// is where it came from when `cwd` is a worktree.
    pub fn set_cwd(&mut self, cwd: PathBuf, origin: Option<PathBuf>) {
        self.cwd = cwd;
        self.origin = origin;
        self.rewrite_system_prompt();
    }

    pub fn context(&self) -> &Arc<Context> {
        &self.context
    }

    /// Nothing has been asked yet.
    pub fn is_empty(&self) -> bool {
        self.title().is_none()
    }

    /// The first line of the first prompt, which names the session in lists.
    /// A command the user ran comes first as `!command`.
    pub fn title(&self) -> Option<String> {
        let mut messages = self.messages.iter();
        while let Some(message) = messages.next() {
            match message {
                Message::User(text) if text == SHELL_PROMPT => {
                    if let Some(Message::Assistant(reply)) = messages.next()
                        && let Some(call) = reply.tool_calls.first()
                    {
                        return Some(format!("!{}", call.summary(&self.cwd)));
                    }
                }
                Message::User(text) => return Some(first_line(text).to_string()),
                _ => {}
            }
        }
        None
    }

    /// Where plan mode writes this session's plan.
    pub fn plan_path(&self) -> PathBuf {
        plan::plan_path(&self.cwd, &self.id)
    }

    /// Later turns go to `model`. The history is kept; only the system
    /// prompt changes, since it names the model.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
        self.rewrite_system_prompt();
    }

    fn rewrite_system_prompt(&mut self) {
        let prompt = self.system_prompt();
        if let Some(first @ Message::System(_)) = self.messages.first_mut() {
            *first = Message::System(prompt);
        }
    }

    fn system_prompt(&self) -> String {
        match self.subagent {
            true => system_prompt::subagent_system_prompt(
                self.persona.as_deref(),
                &self.model,
                &self.cwd,
                &self.extra_dirs,
                &self.context,
            ),
            false => system_prompt(&self.model, &self.cwd, &self.extra_dirs, &self.context),
        }
    }

    /// Rewrites the system prompt when the day it names has passed: a
    /// session that crosses midnight or is resumed later would tell the
    /// model yesterday's date otherwise. Costs one scan of the prompt.
    fn refresh_system_prompt(&mut self) {
        let stale = match self.messages.first() {
            Some(Message::System(prompt)) => !system_prompt::names_today(prompt),
            _ => false,
        };
        if stale {
            self.rewrite_system_prompt();
        }
    }

    /// Adds a user message and runs the turn it starts, until it ends or
    /// `cancel` interrupts it. Tools reach you through `front_end`.
    pub async fn prompt(
        &mut self,
        text: impl Into<String>,
        provider: &dyn Provider,
        tools: &[Box<dyn Tool>],
        front_end: &FrontEnd,
        events: &mpsc::Sender<Event>,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let mut text = text.into();
        // `@explore …` tells the model to delegate to that agent; a subagent
        // has no task tool to do so with.
        if !self.subagent
            && let Some(mention) = subagent::resolve(&text, &self.context.agents, &self.cwd)
        {
            text.push_str(&mention);
        }
        let plan_path = self.plan_path();
        if self.mode == Mode::Plan {
            // The `.nth` directory, with the `.gitignore` that keeps plan
            // files out of git, is ready before the model writes the plan.
            let _ = plan::ensure_dir(&self.cwd).await;
        }
        if let Some(reminder) = self.reminder(&plan_path, front_end).await {
            // On the same message, as opencode adds a synthetic part: two
            // user messages in a row are not something every endpoint takes.
            text = format!("{text}\n\n{reminder}");
        }
        self.last_turn_mode = Some(self.mode);
        self.refresh_system_prompt();
        match self.messages.last_mut() {
            // A turn that failed before the model said anything left the
            // last prompt unanswered; for the same reason as the reminder,
            // this one joins it rather than following it.
            Some(Message::User(previous)) => {
                previous.push_str("\n\n");
                previous.push_str(&text);
            }
            _ => self.messages.push(Message::User(text)),
        }
        self.updated_at = SystemTime::now();
        // The system prompt's files count as loaded, so read never repeats them.
        let mut loaded = self.loaded_instructions.clone();
        loaded.extend(self.context.instructions.iter().map(|i| i.path.clone()));
        let ctx = ToolContext {
            extra_dirs: self.extra_dirs.clone(),
            instructions: Arc::new(Mutex::new(loaded)),
            context: self.context.clone(),
            asker: front_end.asker.clone(),
            screen: front_end.screen.clone(),
            monitors: front_end.monitors.clone(),
            inbox: front_end.inbox.clone(),
            writable: match self.mode {
                Mode::Plan => Writable::Only(plan_path),
                Mode::Act => Writable::Any,
            },
            llm: Llm {
                model: self.model.clone(),
                effort: self.effort,
            },
            workdir: Workdir::new(self.origin.clone()),
            ..ToolContext::new(self.cwd.clone())
        };
        let result = run_turn(
            provider,
            Route {
                model: &self.model,
                effort: self.effort,
                session_id: &self.id.to_string(),
                max_steps: self.max_steps,
            },
            tools,
            &ctx,
            &mut self.messages,
            events,
            cancel,
        )
        .await;
        // Prompts you sent mid-turn joined it.
        self.updated_at = SystemTime::now();
        self.loaded_instructions = ctx
            .instructions
            .lock()
            .expect("loaded instructions lock poisoned")
            .clone();
        if let Some(cwd) = ctx.workdir.moved_to() {
            self.set_cwd(cwd, ctx.workdir.origin());
        }
        result
    }
}

impl Session {
    /// Runs `command` on `shell` for the user, without the model. It lands
    /// in the history as a bash call the model made, after a user message
    /// saying the user ran it, so the model sees it on its next turn.
    /// Cancelling kills the command and still leaves `messages` valid.
    pub async fn shell(
        &mut self,
        command: impl Into<String>,
        shell: &dyn Tool,
        events: &mpsc::Sender<Event>,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let call = ToolCall {
            id: format!("shell-{}", Uuid::new_v4()),
            name: shell.spec().name.into(),
            arguments: serde_json::json!({ "command": command.into() }).to_string(),
        };
        self.messages.push(Message::User(SHELL_PROMPT.into()));
        self.messages.push(Message::Assistant(AssistantMessage {
            tool_calls: vec![call.clone()],
            ..AssistantMessage::default()
        }));
        self.updated_at = SystemTime::now();
        let ctx = ToolContext::new(self.cwd.clone());
        let result = run_call(Some(shell), &ctx, &call, events, cancel).await;
        self.messages.push(Message::ToolResult {
            call_id: call.id,
            content: result.unwrap_or_else(|reason| failed(&reason)),
        });
        match cancel.is_cancelled() {
            true => Err(Error::Interrupted),
            false => Ok(()),
        }
    }
}

impl Session {
    /// What the model needs to hear about the mode before this turn: that
    /// plan mode starts, or that it ended. Nothing while the mode stays.
    async fn reminder(&self, plan_path: &Path, front_end: &FrontEnd) -> Option<String> {
        let entering = self.mode == Mode::Plan && self.last_turn_mode != Some(Mode::Plan);
        let leaving = self.mode == Mode::Act && self.last_turn_mode == Some(Mode::Plan);
        if !entering && !leaving {
            return None;
        }
        // A check that fails reads as no plan yet, which only changes the
        // wording of the reminder.
        let exists = tokio::fs::try_exists(plan_path).await.unwrap_or(false);
        Some(match self.mode {
            Mode::Plan => {
                let approver = match front_end.asker.reaches_someone() {
                    true => Approver::User,
                    false => Approver::Nobody,
                };
                plan::plan_mode_reminder(plan_path, exists, approver)
            }
            Mode::Act => plan::build_switch_reminder(plan_path, exists),
        })
    }
}

fn default_max_steps() -> usize {
    DEFAULT_MAX_STEPS
}

#[cfg(test)]
mod tests {
    use futures::{future::BoxFuture, stream::BoxStream};
    use nth_protocol::{AssistantMessage, BoxError, Listing, Request, StreamEvent, ToolCall};

    use super::*;
    use crate::agent_loop::INTERRUPTED;

    #[test]
    fn starts_with_only_the_system_prompt() {
        let a = Session::new("glm-5.3", ".".into());
        let b = Session::new("glm-5.3", ".".into());

        assert!(matches!(a.messages[..], [Message::System(_)]));
        assert!(a.is_empty());
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn is_titled_by_its_first_prompt() {
        let mut session = Session::new("glm-5.3", ".".into());
        session
            .messages
            .push(Message::User("\n fix the build\nnow".into()));
        session.messages.push(Message::User("and test".into()));

        assert!(!session.is_empty());
        assert_eq!(session.title().as_deref(), Some("fix the build"));
    }

    #[test]
    fn switching_models_rewrites_only_the_system_prompt() {
        let mut session = Session::new("glm-5.3", ".".into());
        session.messages.push(Message::User("go".into()));

        session.set_model("kimi-k3");

        assert_eq!(session.model, "kimi-k3");
        assert_eq!(
            session.messages,
            [
                Message::System(system_prompt(
                    "kimi-k3",
                    ".".as_ref(),
                    &[],
                    &Context::default()
                )),
                Message::User("go".into()),
            ]
        );
    }

    #[test]
    fn added_directories_rewrite_the_prompt_and_survive_the_save() {
        let mut session = Session::new("glm-5.3", "/repo".into());
        session.messages.push(Message::User("go".into()));
        let dirs = vec!["/elsewhere".into()];

        session.set_extra_dirs(dirs.clone());

        let Message::System(prompt) = &session.messages[0] else {
            panic!("starts with the system prompt");
        };
        assert!(prompt.contains("  - /elsewhere\n"), "{prompt}");

        // Saved like the model and the mode are, so `/resume` keeps them.
        let json = serde_json::to_string(&session).expect("serializes");
        let back: Session = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back.extra_dirs, dirs);
        // A session saved before there were extra directories loads none.
        let old = serde_json::json!({
            "id": session.id.to_string(),
            "cwd": "/repo",
            "model": "glm-5.3",
            "created_at": serde_json::to_value(session.created_at).unwrap(),
            "updated_at": serde_json::to_value(session.updated_at).unwrap(),
            "messages": [],
        });
        let old: Session = serde_json::from_value(old).expect("loads");
        assert!(old.extra_dirs.is_empty());
    }

    #[test]
    fn context_goes_into_the_system_prompt_but_not_the_save() {
        let context = Arc::new(Context {
            instructions: vec![nth_context::Instruction {
                path: "/repo/AGENTS.md".into(),
                content: "Be brief.".into(),
            }],
            ..Context::default()
        });
        let mut session = Session::new("glm-5.3", "/repo".into()).with_context(context.clone());
        let Message::System(prompt) = &session.messages[0] else {
            panic!("starts with the system prompt");
        };
        assert!(prompt.ends_with("Instructions from: /repo/AGENTS.md\nBe brief.\n"));

        session.set_model("kimi-k3");
        assert_eq!(
            session.messages[0],
            Message::System(system_prompt("kimi-k3", "/repo".as_ref(), &[], &context)),
            "a new model keeps the instructions"
        );

        let json = serde_json::to_string(&session).expect("serializes");
        let back: Session = serde_json::from_str(&json).expect("deserializes");
        assert!(back.context().instructions.is_empty());
    }

    /// Marks `/repo/sub/AGENTS.md` loaded, as read does when it attaches it.
    struct Claim;

    impl Tool for Claim {
        fn spec(&self) -> nth_protocol::ToolSpec {
            nth_protocol::ToolSpec {
                name: "claim",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            _: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
            let mut loaded = ctx.instructions.lock().expect("not poisoned");
            let fresh = loaded.insert("/repo/sub/AGENTS.md".into());
            Box::pin(async move { Ok(fresh.to_string()) })
        }
    }

    #[tokio::test]
    async fn remembers_the_instruction_files_tools_attached() {
        use crate::agent_loop::tests::{Scripted, call};

        let context = Arc::new(Context {
            instructions: vec![nth_context::Instruction {
                path: "/repo/AGENTS.md".into(),
                content: "root".into(),
            }],
            ..Context::default()
        });
        let mut session = Session::new("glm-5.3", "/repo".into()).with_context(context);
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("1", "claim", ""))],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Claim)];
        let (tx, _rx) = mpsc::channel(64);

        session
            .prompt(
                "go",
                &provider,
                &tools,
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect("turn completes");

        assert_eq!(
            session.loaded_instructions,
            BTreeSet::from(["/repo/AGENTS.md".into(), "/repo/sub/AGENTS.md".into()])
        );
        let json = serde_json::to_string(&session).expect("serializes");
        let back: Session = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back.loaded_instructions, session.loaded_instructions);
    }

    /// Moves the session into `/repo/.nth/worktrees/x`, as enter_worktree
    /// does, or says where it runs when asked with `"pwd"`.
    struct Move;

    impl Tool for Move {
        fn spec(&self) -> nth_protocol::ToolSpec {
            nth_protocol::ToolSpec {
                name: "move",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            args: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
            if args != "pwd" {
                ctx.workdir.enter(&ctx.cwd, "/repo/.nth/worktrees/x".into());
            }
            let cwd = ctx.cwd.display().to_string();
            Box::pin(async move { Ok(cwd) })
        }
    }

    #[tokio::test]
    async fn a_tool_moves_the_next_step_and_the_session() {
        use crate::agent_loop::tests::{Scripted, call};

        let mut session = Session::new("glm-5.3", "/repo".into());
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("1", "move", "{}"))],
            vec![StreamEvent::ToolCall(call("2", "move", r#""pwd""#))],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Move)];
        let (tx, _rx) = mpsc::channel(64);

        session
            .prompt(
                "go",
                &provider,
                &tools,
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect("turn completes");

        let results: Vec<&str> = session
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(results, ["/repo", "/repo/.nth/worktrees/x"]);
        assert_eq!(session.cwd, PathBuf::from("/repo/.nth/worktrees/x"));
        assert_eq!(session.origin, Some("/repo".into()));
        let Some(Message::System(prompt)) = session.messages.first() else {
            panic!("no system prompt");
        };
        assert!(prompt.contains("/repo/.nth/worktrees/x"), "{prompt}");
        let json = serde_json::to_string(&session).expect("serializes");
        let back: Session = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back.origin, session.origin);
    }

    /// Says whether the file it is asked about may be written.
    struct Probe;

    impl Tool for Probe {
        fn spec(&self) -> nth_protocol::ToolSpec {
            nth_protocol::ToolSpec {
                name: "probe",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            args: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
            let path = ctx.cwd.join(args["path"].as_str().unwrap_or_default());
            let checked = ctx.writable.check(&path).map(|()| "writable".to_string());
            Box::pin(async move { checked })
        }
    }

    /// Sends `text` in `session`'s mode with a model that just answers,
    /// and returns the user message the model got.
    async fn send(session: &mut Session, text: &str) -> String {
        let provider = crate::agent_loop::tests::Scripted::new(vec![vec![StreamEvent::TextDelta(
            "ok".into(),
        )]]);
        let (tx, _rx) = mpsc::channel(64);
        session
            .prompt(
                text,
                &provider,
                &[],
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect("turn completes");
        session
            .messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::User(text) => Some(text.clone()),
                _ => None,
            })
            .expect("sent")
    }

    #[tokio::test]
    async fn plan_mode_is_announced_once_and_its_end_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = Session::new("glm-5.3", dir.path().to_path_buf());
        session.mode = Mode::Plan;
        let plan = session.plan_path();

        let first = send(&mut session, "plan it").await;
        assert!(
            first.starts_with("plan it\n\n<system-reminder>\nPlan mode is active."),
            "{first}"
        );
        assert!(first.contains(&format!("create your plan at {}", plan.display())));
        assert!(first.contains("Nobody can approve it"), "headless: {first}");
        assert_eq!(send(&mut session, "more").await, "more");
        assert_eq!(session.title().as_deref(), Some("plan it"));

        std::fs::create_dir_all(plan.parent().expect("dir")).expect("dirs");
        std::fs::write(&plan, "# Plan").expect("writes");
        session.mode = Mode::Act;
        let switched = send(&mut session, "go").await;
        assert!(
            switched.starts_with(
                "go\n\n<system-reminder>\nYour operational mode has changed from plan to act."
            ),
            "{switched}"
        );
        assert!(switched.ends_with(&format!(
            "A plan file exists at {}. You should execute on the plan defined within it",
            plan.display()
        )));
        assert_eq!(send(&mut session, "next").await, "next");
    }

    #[tokio::test]
    async fn plan_mode_keeps_a_gitignore_in_the_project_nth() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = Session::new("glm-5.3", dir.path().to_path_buf());
        session.mode = Mode::Plan;

        send(&mut session, "plan it").await;

        let gitignore = std::fs::read_to_string(dir.path().join(".nth/.gitignore")).expect("reads");
        assert!(gitignore.contains("plans/"), "{gitignore}");
        assert!(gitignore.contains(".gitignore"), "{gitignore}");
    }

    /// Fails every request, as a provider that is down does.
    struct Down;

    impl Provider for Down {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            Box::pin(async { Ok(Listing::default()) })
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            Box::pin(async { Err("connection refused".into()) })
        }
    }

    #[tokio::test]
    async fn a_prompt_after_a_failed_turn_joins_the_unanswered_one() {
        let mut session = Session::new("glm-5.3", "/repo".into());
        let (tx, _rx) = mpsc::channel(64);

        let failed = session
            .prompt(
                "first",
                &Down,
                &[],
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await;

        assert!(matches!(failed, Err(Error::Provider(_))));
        assert_eq!(session.messages[1..], [Message::User("first".into())]);

        // One user message, as endpoints that reject two in a row expect.
        assert_eq!(send(&mut session, "second").await, "first\n\nsecond");
        assert!(matches!(
            session.messages[1..],
            [Message::User(_), Message::Assistant(_)]
        ));
        assert_eq!(session.title().as_deref(), Some("first"));
    }

    #[tokio::test]
    async fn a_prompt_rewrites_a_system_prompt_from_another_day() {
        let mut session = Session::new("glm-5.3", "/repo".into());
        let current = session.messages[0].clone();
        let stale = system_prompt("glm-5.3", "/repo".as_ref(), &[], &Context::default())
            .replace("Today's date: ", "Today's date: Mon Jan 01 2001, not ");
        session.messages[0] = Message::System(stale);

        send(&mut session, "go").await;

        assert_eq!(session.messages[0], current);
    }

    #[tokio::test]
    async fn act_mode_from_the_start_says_nothing() {
        let mut session = Session::new("glm-5.3", "/repo".into());
        assert_eq!(send(&mut session, "go").await, "go");
    }

    #[tokio::test]
    async fn plan_mode_lets_tools_write_only_the_plan() {
        use crate::agent_loop::tests::{Scripted, call};

        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = Session::new("glm-5.3", dir.path().to_path_buf());
        session.mode = Mode::Plan;
        let plan = format!(".nth/plans/{}.md", session.id);
        let provider = Scripted::new(vec![
            vec![
                StreamEvent::ToolCall(call("1", "probe", r#"{"path":"src/main.rs"}"#)),
                StreamEvent::ToolCall(call("2", "probe", &format!(r#"{{"path":"{plan}"}}"#))),
            ],
            vec![StreamEvent::TextDelta("done".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Probe)];
        let (tx, _rx) = mpsc::channel(64);

        session
            .prompt(
                "plan",
                &provider,
                &tools,
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect("turn completes");

        let results: Vec<&str> = session
            .messages
            .iter()
            .filter_map(|m| match m {
                Message::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            results[0].starts_with("Error: plan mode is active"),
            "{}",
            results[0]
        );
        assert_eq!(results[1], "writable");
    }

    #[test]
    fn a_save_from_before_modes_loads_as_act() {
        let mut session = Session::new("glm-5.3", ".".into());
        session.mode = Mode::Plan;
        let mut json = serde_json::to_value(&session).expect("serializes");
        assert_eq!(json["mode"], "plan");
        let back: Session = serde_json::from_value(json.clone()).expect("deserializes");
        assert_eq!(back.mode, Mode::Plan);

        let fields = json.as_object_mut().expect("an object");
        fields.remove("mode");
        fields.remove("last_turn_mode");
        let old: Session = serde_json::from_value(json).expect("deserializes");
        assert_eq!(old.mode, Mode::Act);
    }

    #[test]
    fn round_trips_through_json() {
        let mut session = Session::new("glm-5.3", ".".into());
        session.messages.extend([
            Message::User("go".into()),
            Message::Assistant(AssistantMessage {
                text: "on it".into(),
                reasoning: "think".into(),
                tool_calls: vec![ToolCall {
                    id: "1".into(),
                    name: "read".into(),
                    arguments: r#"{"path":"x"}"#.into(),
                }],
            }),
            Message::ToolResult {
                call_id: "1".into(),
                content: "hi".into(),
            },
        ]);

        let json = serde_json::to_string(&session).expect("serializes");
        let back: Session = serde_json::from_str(&json).expect("deserializes");

        assert_eq!(back, session);
    }

    /// Says what it was asked to run, or never finishes on `hang`.
    struct Shell;

    impl Tool for Shell {
        fn spec(&self) -> nth_protocol::ToolSpec {
            nth_protocol::ToolSpec {
                name: "bash",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            args: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
            use futures::FutureExt;
            async move {
                let command = args["command"].as_str().unwrap_or_default().to_string();
                if command == "hang" {
                    std::future::pending::<()>().await;
                }
                ctx.output.send(command.clone()).await;
                Ok(format!("ran {command}"))
            }
            .boxed()
        }
    }

    #[tokio::test]
    async fn a_shell_command_lands_as_a_bash_call() {
        let mut session = Session::new("glm-5.3", ".".into());
        let (events, mut rx) = mpsc::channel(16);

        let result = session
            .shell("ls", &Shell, &events, &CancellationToken::new())
            .await;

        assert!(result.is_ok());
        let [
            _,
            Message::User(said),
            Message::Assistant(reply),
            Message::ToolResult { call_id, content },
        ] = &session.messages[..]
        else {
            panic!("unexpected messages: {:?}", session.messages);
        };
        assert_eq!(said, SHELL_PROMPT);
        assert_eq!(reply.tool_calls[0].name, "bash");
        assert_eq!(reply.tool_calls[0].arguments, r#"{"command":"ls"}"#);
        assert_eq!(call_id, &reply.tool_calls[0].id);
        assert_eq!(content, "ran ls");
        assert_eq!(session.title().as_deref(), Some("!ls"));
        assert!(matches!(rx.recv().await, Some(Event::ToolStarted(_))));
        assert!(matches!(rx.recv().await, Some(Event::ToolOutput { .. })));
        assert!(matches!(rx.recv().await, Some(Event::ToolFinished { .. })));
    }

    #[tokio::test]
    async fn cancelling_a_shell_command_answers_its_call() {
        let mut session = Session::new("glm-5.3", ".".into());
        let (events, _rx) = mpsc::channel(16);
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = session.shell("hang", &Shell, &events, &cancel).await;

        assert!(matches!(result, Err(Error::Interrupted)));
        assert!(matches!(
            session.messages.last(),
            Some(Message::ToolResult { content, .. }) if *content == failed(INTERRUPTED)
        ));
    }
}
