use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::SystemTime,
};

use nth_context::{Agent, Context, instructions::Watch};
use nth_protocol::{
    AssistantMessage, Cost, Effort, Event, FrontEnd, Llm, Message, Mode, Provider, SubagentLlm,
    Tool, ToolCall, ToolContext, Usage, Workdir, instructions_notice,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{
    Compaction, DEFAULT_MAX_STEPS, Error, Ledger, Route, Spend, Turn,
    agent_loop::{emit, run_call},
    memory,
    plan::{self, Approver},
    primary, subagent, system_prompt, work,
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
    /// plan mode or leaves it; `None` before the first turn and after one
    /// run as a primary agent.
    #[serde(default)]
    last_turn_mode: Option<Mode>,
    /// The primary agent the session runs as, in place of `mode`; `None`
    /// runs in `mode`. An agent no longer found runs in `mode` too.
    #[serde(default)]
    pub agent: Option<String>,
    /// The primary agent the last turn ran as, so the model is told when
    /// it enters one or leaves it.
    #[serde(default)]
    last_turn_agent: Option<String>,
    pub created_at: SystemTime,
    /// When a prompt was last sent; saved sessions are listed by it.
    pub updated_at: SystemTime,
    pub messages: Vec<Message>,
    /// Instruction files the read tool has attached, so a resumed session
    /// does not get them again.
    #[serde(default)]
    pub loaded_instructions: BTreeSet<PathBuf>,
    /// What every turn spent, its subagents' turns included once a
    /// front-end hands them over. Sessions saved before it load with none.
    #[serde(default)]
    pub usage: Ledger,
    /// The tokens the last reply reported it used, so a resumed session
    /// still says how full the window was and compacts before its first
    /// request when it is over; sessions saved before it load with none.
    /// Cleared by a compaction, whose checkpoint is smaller than what it
    /// replaced.
    #[serde(default)]
    pub last_usage: Option<Usage>,
    /// How the session keeps itself inside the model's window. Comes from
    /// the config's `[context]`, not the save, so a resumed session follows
    /// today's config, as `max_steps` does.
    #[serde(skip)]
    pub compaction: Compaction,
    /// The model's context window, when the front-end knows it; without it
    /// nothing compacts on its own. Saved, so a resumed session keeps the
    /// window its model was listed with.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// The model's maximum output tokens, for the reserved buffer's
    /// default; `None` falls back to 20k.
    #[serde(default)]
    pub max_output: Option<u64>,
    /// Model requests a turn may make before it gives up. Comes from the
    /// config, not the save, so a resumed session follows today's config.
    #[serde(skip, default = "default_max_steps")]
    pub max_steps: usize,
    /// Whether the model takes images, as the listing says; the front-end
    /// sets it with the limits. Unknown counts as yes: the providers still
    /// turn an image into a note for a model that takes none.
    #[serde(skip, default = "yes")]
    pub takes_images: bool,
    /// What the model costs, when the front-end's listing says; stamped onto
    /// every spend it records so the price survives without a listing.
    #[serde(skip)]
    pub cost: Option<Cost>,
    /// The folder the read tool keeps image files in, which the front-end
    /// sets from the store; a subagent's is its parent's. Not saved.
    #[serde(skip)]
    pub images: Option<PathBuf>,
    /// What the subagents this session starts run on; `None` runs them on
    /// its own model. Set by the front-end before each turn, not saved.
    #[serde(skip)]
    pub subagent_llm: Option<SubagentLlm>,
    /// What the built-in clerk runs on; `None` runs it as the other
    /// subagents. Set by the front-end before each turn, not saved.
    #[serde(skip)]
    pub clerk_llm: Option<SubagentLlm>,
    /// Runs for another session's model rather than for you: it has no
    /// task tool, so it is told of no agents and `@name` means nothing.
    #[serde(default)]
    pub subagent: bool,
    /// The session whose model started this one, for a subagent; its
    /// requests name it, so they can be told apart from the parent's.
    #[serde(default)]
    pub parent: Option<Uuid>,
    /// A subagent's own system prompt, in place of the model's persona;
    /// `None` keeps the persona.
    #[serde(default)]
    persona: Option<String>,
    /// Not saved: it is read afresh for the working directory, so a resumed
    /// session sees the instruction files as they are now.
    #[serde(skip)]
    context: Arc<Context>,
    /// What the model last saw of the instruction files, from `context`
    /// on; a change reaches it at its next step.
    #[serde(skip)]
    watch: Watch,
}

impl Session {
    pub fn new(model: impl Into<String>, cwd: PathBuf) -> Self {
        let model = model.into();
        let context = Arc::<Context>::default();
        let id = Uuid::new_v4();
        let messages = vec![Message::System(system_prompt(
            None,
            id,
            &model,
            &cwd,
            &[],
            &context,
        ))];
        let now = SystemTime::now();
        Self {
            id,
            cwd,
            origin: None,
            extra_dirs: Vec::new(),
            model,
            effort: Effort::default(),
            mode: Mode::default(),
            last_turn_mode: None,
            agent: None,
            last_turn_agent: None,
            created_at: now,
            updated_at: now,
            messages,
            loaded_instructions: BTreeSet::new(),
            usage: Ledger::default(),
            last_usage: None,
            compaction: Compaction::default(),
            context_window: None,
            max_output: None,
            max_steps: DEFAULT_MAX_STEPS,
            takes_images: true,
            cost: None,
            images: None,
            subagent_llm: None,
            clerk_llm: None,
            subagent: false,
            parent: None,
            persona: None,
            context,
            watch: Watch::default(),
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
        self.watch = Watch::new(&context);
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

    /// Runs later turns as the primary agent `agent`, or in `mode` when
    /// `None`. The agent's prompt replaces the model's persona.
    pub fn set_agent(&mut self, agent: Option<String>) {
        self.agent = agent;
        self.rewrite_system_prompt();
    }

    /// The primary agent the session runs as, when it is still found; a
    /// subagent runs as none.
    pub fn primary(&self) -> Option<&Agent> {
        let name = self.agent.as_deref().filter(|_| !self.subagent)?;
        self.context.agents.primary(name)
    }

    /// The mode the session's tools follow: a primary agent acts, and what
    /// it may change is up to its tools and its prompt.
    fn tools_mode(&self) -> Mode {
        match self.primary() {
            Some(_) => Mode::Act,
            None => self.mode,
        }
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

    /// Where this session's orchestration keeps its work items: under the
    /// main checkout, wherever a worktree tool moved the session.
    pub fn work_dir(&self) -> PathBuf {
        let root = work::root(self.origin.as_deref(), &self.cwd);
        work::work_dir(&root, &self.id.to_string())
    }

    /// Later turns go to `model`. The history is kept; only the system
    /// prompt changes, since it names the model.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
        self.rewrite_system_prompt();
    }

    /// The limits the model was listed with: its context window and output
    /// limit, which size the compaction trigger. The front-end sets them
    /// whenever it knows them, so they follow a model switch.
    pub fn set_limits(&mut self, context_window: Option<u64>, max_output: Option<u64>) {
        self.context_window = context_window;
        self.max_output = max_output;
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
                self.id,
                &self.model,
                &self.cwd,
                &self.extra_dirs,
                &self.context,
            ),
            false => system_prompt(
                self.primary().and_then(|agent| agent.prompt.as_deref()),
                self.id,
                &self.model,
                &self.cwd,
                &self.extra_dirs,
                &self.context,
            ),
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
        let agent = self.primary().map(|agent| agent.name.clone());
        if self.tools_mode() == Mode::Plan {
            // The `.nth` directory, with the `.gitignore` that keeps plan
            // files out of git, is ready before the model writes the plan.
            let _ = plan::ensure_dir(&self.cwd).await;
        }
        if let Some(dir) = self.memory_dir() {
            // Kept out of git before the model first saves to it.
            let _ = memory::ensure_dir(&dir, &self.cwd).await;
        }
        let approver = match front_end.asker.reaches_someone() {
            true => Approver::User,
            false => Approver::Nobody,
        };
        let first = !self.messages.iter().any(|m| matches!(m, Message::User(_)));
        if let Some(reminder) =
            primary::reminder(agent.as_deref(), self.last_turn_agent.as_deref(), first)
        {
            text = format!("{text}\n\n{reminder}");
        }
        // A primary agent's own prompt says how it works; plan mode's
        // reminders are for plan and act.
        if agent.is_none()
            && let Some(reminder) = plan::reminder(
                self.mode,
                self.last_turn_mode,
                &plan_path,
                self.memory_dir().as_deref(),
                approver,
            )
            .await
        {
            // On the same message, as opencode adds a synthetic part: two
            // user messages in a row are not something every endpoint takes.
            text = format!("{text}\n\n{reminder}");
        }
        // An instruction file edited since the last turn, in front of the
        // prompt as the front-end's notices are.
        let changes = self.watch.changes(self.loaded()).await;
        if let Some(changed) = instructions_notice(&changes) {
            text = format!("{changed}\n{text}");
            emit(events, Event::Notice(changed)).await;
        }
        self.last_turn_mode = match agent {
            Some(_) => None,
            None => Some(self.mode),
        };
        self.last_turn_agent = agent;
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
        let mut held = self.held(front_end);
        let result = self
            .turn(&mut held, provider, tools, events, cancel)
            .run()
            .await;
        let Held { ctx, spend, .. } = held;
        self.usage.record(spend);
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

    /// What a turn of this session needs held for as long as it runs.
    fn held(&self, front_end: &FrontEnd) -> Held {
        let mut spend = Spend::new(&self.model, None);
        spend.cost = self.cost;
        Held {
            ctx: self.tool_context(front_end),
            spend,
            session_id: self.id.to_string(),
            parent_session_id: self.parent.map(|p| p.to_string()),
        }
    }

    /// A turn on this session's history and route, with what `held` holds.
    fn turn<'a>(
        &'a mut self,
        held: &'a mut Held,
        provider: &'a dyn Provider,
        tools: &'a [Box<dyn Tool>],
        events: &'a mpsc::Sender<Event>,
        cancel: &'a CancellationToken,
    ) -> Turn<'a> {
        Turn {
            provider,
            route: Route {
                model: &self.model,
                effort: self.effort,
                session_id: &held.session_id,
                parent_session_id: held.parent_session_id.as_deref(),
                max_steps: self.max_steps,
                // The window and the output limit are the front-end's to
                // fill; without them nothing compacts on its own.
                context_window: self.context_window,
                max_output: self.max_output,
                compaction: self.compaction,
            },
            tools,
            ctx: &held.ctx,
            messages: &mut self.messages,
            spend: &mut held.spend,
            last_usage: &mut self.last_usage,
            events,
            cancel,
        }
    }

    /// What the model's tools and the front-end reach this session through.
    fn tool_context(&self, front_end: &FrontEnd) -> ToolContext {
        ToolContext {
            extra_dirs: self.extra_dirs.clone(),
            instructions: Arc::new(Mutex::new(self.loaded())),
            watch: self.watch.clone(),
            context: self.context.clone(),
            asker: front_end.asker.clone(),
            screen: front_end.screen.clone(),
            monitors: front_end.monitors.clone(),
            inbox: front_end.inbox.clone(),
            writable: plan::writable(self.tools_mode(), self.plan_path(), self.memory_dir()),
            agent: self.primary().cloned(),
            llm: Llm {
                model: self.model.clone(),
                effort: self.effort,
            },
            subagent: self.subagent_llm.clone(),
            clerk: self.clerk_llm.clone(),
            context_window: self.context_window,
            max_output: self.max_output,
            compaction: self.compaction,
            workdir: Workdir::new(self.origin.clone()),
            session: self.id.to_string(),
            takes_images: self.takes_images,
            images: self.images.clone(),
            ..ToolContext::new(self.cwd.clone())
        }
    }

    /// The memory folder this session keeps, when it keeps one: a subagent
    /// works for another model, which keeps it.
    fn memory_dir(&self) -> Option<PathBuf> {
        let memory = self.context.memory.as_ref().filter(|_| !self.subagent)?;
        Some(memory.dir.clone())
    }

    /// The instruction files in the conversation: the system prompt's, so
    /// read never repeats them, and those tools attached.
    fn loaded(&self) -> BTreeSet<PathBuf> {
        let mut loaded = self.loaded_instructions.clone();
        loaded.extend(self.context.instructions.iter().map(|i| i.path.clone()));
        loaded
    }

    /// Runs one compaction outside a turn: everything the requests would
    /// not keep verbatim becomes a summary they start from. The `/compact`
    /// command and the loop's overflow recovery share `Turn::compact`;
    /// this is the manual entry.
    pub async fn compact(
        &mut self,
        provider: &dyn Provider,
        tools: &[Box<dyn Tool>],
        front_end: &FrontEnd,
        events: &mpsc::Sender<Event>,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        let mut held = self.held(front_end);
        // Only `Turn::run` writes the trigger's usage: a compaction's is the
        // summary request's, not the conversation's size.
        let result = self
            .turn(&mut held, provider, tools, events, cancel)
            .compact()
            .await;
        let spend = held.spend;
        if matches!(result, Ok(true)) {
            // The conversation the old usage described is gone; keeping it
            // would fire the trigger again at once.
            self.last_usage = None;
        }
        self.usage.record(spend);
        self.updated_at = SystemTime::now();
        result.map(|_| ())
    }
}

/// What a turn borrows that the session does not hold.
struct Held {
    ctx: ToolContext,
    spend: Spend,
    session_id: String,
    parent_session_id: Option<String>,
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
        let answer = run_call(Some(shell), &ctx, &call, events, cancel).await;
        self.messages.push(answer);
        match cancel.is_cancelled() {
            true => Err(Error::Interrupted),
            false => Ok(()),
        }
    }
}

fn yes() -> bool {
    true
}

fn default_max_steps() -> usize {
    DEFAULT_MAX_STEPS
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::Path;

    use nth_protocol::{AssistantMessage, INTERRUPTED, StreamEvent, ToolCall, Usage, failed};

    use super::*;

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
                    None,
                    session.id,
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
            Message::System(system_prompt(
                None,
                session.id,
                "kimi-k3",
                "/repo".as_ref(),
                &[],
                &context
            )),
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
        let (tx, mut rx) = mpsc::channel(64);

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
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event {
                Event::Moved(_) | Event::ToolFinished { .. } => events.push(event),
                _ => {}
            }
        }
        // Only the call that moved says so, before it finishes.
        assert!(matches!(
            &events[..],
            [Event::Moved(cwd), Event::ToolFinished { .. }, Event::ToolFinished { .. }]
                if cwd == Path::new("/repo/.nth/worktrees/x")
        ));
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

    /// Sends `text` in `session`'s mode with a model that just answers,
    /// and returns the user message the model got.
    pub(crate) async fn send(session: &mut Session, text: &str) -> String {
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

    fn with_agents_md(dir: &std::path::Path, text: &str) -> Session {
        std::fs::write(dir.join("AGENTS.md"), text).expect("writes");
        let context = Context::discover(dir, &nth_context::Paths::default());
        Session::new("glm-5.3", dir.to_path_buf()).with_context(Arc::new(context))
    }

    #[tokio::test]
    async fn an_instruction_file_edited_between_turns_comes_before_the_next_prompt() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = with_agents_md(dir.path(), "old");
        assert_eq!(send(&mut session, "first").await, "first");

        std::fs::write(dir.path().join("AGENTS.md"), "new").expect("writes");
        let path = dir.path().join("AGENTS.md").display().to_string();
        assert_eq!(
            send(&mut session, "second").await,
            format!(
                "<instructions path=\"{path}\" change=\"edited\" content=\"whole\">\nnew\n</instructions>\nsecond"
            )
        );
        assert_eq!(send(&mut session, "third").await, "third", "told once");
    }

    /// Rewrites the working directory's `AGENTS.md`, as the model would.
    struct Rewrite;

    impl Tool for Rewrite {
        fn spec(&self) -> nth_protocol::ToolSpec {
            nth_protocol::ToolSpec {
                name: "rewrite",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            _: serde_json::Value,
            ctx: &'a ToolContext,
        ) -> futures::future::BoxFuture<'a, nth_protocol::ToolResult> {
            let path = ctx.cwd.join("AGENTS.md");
            Box::pin(async move {
                tokio::fs::write(path, "rewritten")
                    .await
                    .map_err(|e| e.to_string())?;
                Ok("written".into())
            })
        }
    }

    #[tokio::test]
    async fn an_instruction_file_a_tool_edits_reaches_the_next_step() {
        use crate::agent_loop::tests::{Scripted, call};

        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = with_agents_md(dir.path(), "original");
        let provider = Scripted::new(vec![
            vec![StreamEvent::ToolCall(call("1", "rewrite", ""))],
            vec![StreamEvent::TextDelta("noted".into())],
        ]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Rewrite)];
        let (tx, mut rx) = mpsc::channel(64);

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

        let Some(Message::User(after)) = session.messages.get(4) else {
            panic!("a message after the result: {:?}", session.messages);
        };
        assert!(
            after.starts_with("<instructions ") && after.contains("change=\"edited\""),
            "{after}"
        );
        assert!(after.contains("rewritten"), "{after}");
        drop(tx);
        let mut shown = false;
        while let Some(event) = rx.recv().await {
            shown |= matches!(event, Event::Notice(text) if text == *after);
        }
        assert!(shown, "the front-end hears it");
    }

    #[tokio::test]
    async fn a_session_keeps_its_memory_out_of_git_and_writable_while_planning() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join(".git")).expect("dir");
        let paths = nth_context::Paths {
            memory: true,
            ..nth_context::Paths::default()
        };
        let context = Arc::new(Context::discover(dir.path(), &paths));
        let mut session =
            Session::new("glm-5.3", dir.path().to_path_buf()).with_context(context.clone());
        session.mode = Mode::Plan;

        send(&mut session, "plan it").await;

        let memory = dir.path().join(".nth/memory");
        assert!(memory.join(".gitignore").is_file());
        let ctx = session.tool_context(&FrontEnd::default());
        assert_eq!(ctx.writable.check(&memory.join("terse.md")), Ok(()));
        assert!(ctx.writable.check(&dir.path().join("src/main.rs")).is_err());

        let subagent = Session::new("glm-5.3", dir.path().to_path_buf())
            .with_context(context)
            .as_subagent(None);
        assert_eq!(subagent.memory_dir(), None, "the parent keeps it");
    }

    /// A project with the primary agent `review`, which may only read and
    /// echo.
    fn with_reviewer(dir: &std::path::Path) -> Arc<Context> {
        std::fs::create_dir(dir.join(".git")).expect("dir");
        let agents = dir.join(".nth/agents");
        std::fs::create_dir_all(&agents).expect("dir");
        std::fs::write(
            agents.join("review.md"),
            "---\ndescription: Reviews\nmode: primary\ntools: read, echo\n---\nYou review.\n",
        )
        .expect("writes");
        Arc::new(Context::discover(dir, &nth_context::Paths::default()))
    }

    #[tokio::test]
    async fn a_primary_agent_runs_in_place_of_the_mode() {
        use crate::agent_loop::tests::{Echo, Named, Scripted};

        let dir = tempfile::tempdir().expect("tempdir");
        let mut session = Session::new("glm-5.3", dir.path().to_path_buf())
            .with_context(with_reviewer(dir.path()));
        session.mode = Mode::Plan;
        session.set_agent(Some("review".into()));

        let Message::System(system) = &session.messages[0] else {
            panic!("a system prompt first");
        };
        assert!(system.starts_with("You review.\n"), "{system}");
        let ctx = session.tool_context(&FrontEnd::default());
        assert!(!ctx.writable.planning(), "the agent acts");
        assert_eq!(ctx.writable.check(&dir.path().join("src/main.rs")), Ok(()));
        assert_eq!(ctx.agent.as_ref().map(|a| a.name.as_str()), Some("review"));

        let provider = Scripted::new(vec![vec![StreamEvent::TextDelta("ok".into())]]);
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Echo), Box::new(Named("bash"))];
        let (tx, _rx) = mpsc::channel(64);
        session
            .prompt(
                "look",
                &provider,
                &tools,
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect("turn completes");
        assert_eq!(
            *provider.tools.lock().expect("not poisoned"),
            [vec!["echo".to_string()]],
            "only the tools the agent allows"
        );
        assert_eq!(
            session.messages[1],
            Message::User("look".into()),
            "no reminder on the first turn, and no plan mode"
        );

        session.set_agent(None);
        let back = send(&mut session, "plan now").await;
        assert!(
            back.contains("no longer running as the review agent"),
            "{back}"
        );
        assert!(back.contains("Plan mode is active"), "{back}");

        session.set_agent(Some("review".into()));
        let again = send(&mut session, "review again").await;
        assert!(again.contains("now running as the review agent"), "{again}");
        assert!(!again.contains("Plan mode"), "{again}");
    }

    #[test]
    fn an_agent_that_is_gone_runs_in_the_mode() {
        let mut session = Session::new("glm-5.3", "/repo".into());
        session.mode = Mode::Plan;
        session.set_agent(Some("missing".into()));

        assert_eq!(session.primary(), None);
        assert_eq!(session.tools_mode(), Mode::Plan);
    }

    #[test]
    fn the_work_plan_stays_in_the_main_checkout() {
        let mut session = Session::new("glm", "/repo".into());
        let before = session.work_dir();

        session.set_cwd("/repo/.nth/worktrees/feat".into(), Some("/repo".into()));

        assert_eq!(session.work_dir(), before);
        assert!(before.starts_with("/repo/.nth/work/"));
    }

    #[tokio::test]
    async fn a_prompt_after_a_failed_turn_joins_the_unanswered_one() {
        use crate::agent_loop::tests::{Failure, Reply, Scripted};

        let mut session = Session::new("glm-5.3", "/repo".into());
        let (tx, _rx) = mpsc::channel(64);

        let failed = session
            .prompt(
                "first",
                &Scripted::always(Reply::Refused(Failure::Down)),
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
        let stale = system_prompt(
            None,
            session.id,
            "glm-5.3",
            "/repo".as_ref(),
            &[],
            &Context::default(),
        )
        .replace("Today's date: ", "Today's date: Mon Jan 01 2001, not ");
        session.messages[0] = Message::System(stale);

        send(&mut session, "go").await;

        assert_eq!(session.messages[0], current);
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
                images: Vec::new(),
            },
            Message::Compaction {
                summary: "so far".into(),
                recent: "[User]: go".into(),
            },
        ]);
        session.compaction.auto = false;
        session.compaction.reserved = 1_000;
        session.compaction.preserve = 2_000;
        session.last_usage = Some(Usage {
            input: 10,
            output: 2,
            ..Usage::default()
        });

        let json = serde_json::to_string(&session).expect("serializes");
        let mut back: Session = serde_json::from_str(&json).expect("deserializes");

        assert_eq!(
            back.compaction,
            Compaction::default(),
            "the policy is the config's, never the save's"
        );
        back.compaction = session.compaction;
        assert_eq!(back, session);
    }

    #[test]
    fn a_save_from_before_compaction_loads_with_the_defaults() {
        let session = Session::new("glm-5.3", ".".into());
        let mut json = serde_json::to_value(&session).expect("serializes");
        let fields = json.as_object_mut().expect("an object");
        fields.remove("last_usage");
        fields.remove("compaction");
        fields.remove("context_window");
        fields.remove("max_output");

        let old: Session = serde_json::from_value(json).expect("deserializes");

        assert_eq!(old.last_usage, None);
        assert_eq!(old.compaction, Compaction::default());
        assert_eq!(old.context_window, None);
        assert_eq!(old.max_output, None);
    }

    #[test]
    fn limits_are_set_and_saved() {
        let mut session = Session::new("glm-5.3", ".".into());

        session.set_limits(Some(200_000), Some(64_000));

        let json = serde_json::to_string(&session).expect("serializes");
        let back: Session = serde_json::from_str(&json).expect("deserializes");
        assert_eq!(back.context_window, Some(200_000));
        assert_eq!(back.max_output, Some(64_000));
    }

    #[tokio::test]
    async fn a_turn_records_the_usage_it_reported() {
        use crate::agent_loop::tests::Scripted;

        let mut session = Session::new("glm-5.3", "/repo".into());
        let provider = Scripted::new(vec![vec![
            StreamEvent::TextDelta("ok".into()),
            StreamEvent::Usage(Usage {
                input: 100,
                output: 10,
                ..Usage::default()
            }),
        ]]);
        let (tx, _rx) = mpsc::channel(64);

        session
            .prompt(
                "go",
                &provider,
                &[],
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect("turn completes");

        assert_eq!(
            session.last_usage,
            Some(Usage {
                input: 100,
                output: 10,
                ..Usage::default()
            })
        );
    }

    #[tokio::test]
    async fn a_manual_compaction_summarizes_and_records_its_spend() {
        use crate::agent_loop::tests::Scripted;

        let mut session = Session::new("glm-5.3", "/repo".into());
        session.messages.push(Message::User("go".into()));
        session.messages.push(Message::Assistant(AssistantMessage {
            text: "done".into(),
            ..AssistantMessage::default()
        }));
        let provider = Scripted::new(vec![vec![
            StreamEvent::TextDelta("the summary".into()),
            StreamEvent::Usage(Usage {
                input: 50,
                output: 5,
                ..Usage::default()
            }),
        ]]);
        let (tx, _rx) = mpsc::channel(64);

        session
            .compact(
                &provider,
                &[],
                &FrontEnd::default(),
                &tx,
                &CancellationToken::new(),
            )
            .await
            .expect("compacts");

        assert_eq!(
            session.messages.last(),
            Some(&Message::Compaction {
                summary: "the summary".into(),
                recent: "[Assistant]: done".into(),
            })
        );
        assert_eq!(
            session.last_usage, None,
            "the summary request's own size is not the conversation's, and \
             what was compacted is gone"
        );
        assert_eq!(session.usage.total().steps, 1, "the spend is still counted");
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
            Message::ToolResult {
                call_id, content, ..
            },
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
