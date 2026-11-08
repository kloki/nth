use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime},
};

use nth_context::{Agent, Context, instructions::Watch};
use nth_protocol::{Cost, Effort, Listing, Message, Mode, ModelSettings, Usage};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    Compaction, DEFAULT_MAX_STEPS, DEFAULT_TOOL_TIMEOUT, Ledger, plan, system_prompt, work,
};

mod shell;
mod turn;

pub(crate) use turn::TurnInput;

fn first_line(text: &str) -> &str {
    text.trim().lines().next().unwrap_or_default()
}

/// The user message before a command the user ran themselves, as opencode
/// words it. Front-ends that replay a session leave it out.
pub const SHELL_PROMPT: &str = "The following tool was executed by the user";

/// What a front-end sets before each turn and never saves: today's config,
/// so a resumed session follows it, and what the front-end's listing says
/// of the models, which a later listing may say differently.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnEnv {
    /// How the session keeps itself inside the model's window.
    pub compaction: Compaction,
    /// Model requests a turn may make before it gives up.
    pub max_steps: usize,
    /// How long one tool call may run before the loop stops it; `None`
    /// never.
    pub tool_timeout: Option<Duration>,
    /// Whether the model takes images. Unknown counts as yes: the providers
    /// still turn an image into a note for a model that takes none.
    pub takes_images: bool,
    /// What the model costs, when the listing says; stamped onto every
    /// spend so the price survives without a listing.
    pub cost: Option<Cost>,
    /// The folder the read tool keeps image files in, from the store; a
    /// subagent's is its parent's.
    pub images: Option<PathBuf>,
    /// What the subagents this session starts run on; `None` runs them on
    /// its own model.
    pub subagent: Option<ModelSettings>,
    /// What the built-in clerk runs on; `None` runs it as the other
    /// subagents.
    pub clerk: Option<ModelSettings>,
}

impl Default for TurnEnv {
    fn default() -> Self {
        Self {
            compaction: Compaction::default(),
            max_steps: DEFAULT_MAX_STEPS,
            tool_timeout: Some(DEFAULT_TOOL_TIMEOUT),
            takes_images: true,
            cost: None,
            images: None,
            subagent: None,
            clerk: None,
        }
    }
}

/// One conversation: who it runs for, where, and everything said so far.
/// Serializable so the [`Store`](crate::Store) can persist and resume it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// The main checkout of `state_root`, where the session's Curios store
    /// is, as last found: kept so deleting the session and acknowledging its
    /// feedback find the store without asking git, even once the repository
    /// has gone. Never decides where the store is; `state_root` does.
    #[serde(default)]
    pub curio_root: Option<PathBuf>,
    /// Whether this run already looked for `curio_root`.
    #[serde(skip)]
    pub(crate) curio_root_found: bool,
    /// Whether the store was readied for this turn, which alone lets plan
    /// mode name and write the source folder: `curio_root` may be a cache
    /// kept for a repository that is gone.
    #[serde(skip)]
    pub(crate) curio_source_ready: bool,
    /// Receipts persisted alongside the history which contains their notices.
    #[serde(default)]
    pub curio_receipts: BTreeSet<String>,
    /// Also sent to the provider so it can route and cache per conversation.
    pub id: Uuid,
    pub cwd: PathBuf,
    /// The directory the session left to work in a worktree, which
    /// `exit_worktree` goes back to; `None` outside one.
    #[serde(default)]
    pub origin: Option<PathBuf>,
    /// The checkout owning this session's generated state, even if cwd disappears.
    #[serde(default)]
    pub state_root: Option<PathBuf>,
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
    /// The model's context window, when the front-end knows it; without it
    /// nothing compacts on its own. Saved, so a resumed session keeps the
    /// window its model was listed with.
    #[serde(default)]
    pub context_window: Option<u64>,
    /// The model's maximum output tokens, for the reserved buffer's
    /// default; `None` falls back to 20k.
    #[serde(default)]
    pub max_output: Option<u64>,
    /// Set by the front-end before each turn, so it follows today's config
    /// and listing rather than the save's.
    #[serde(skip)]
    pub env: TurnEnv,
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
        let state_root = Some(nth_context::main_project_root(&cwd).unwrap_or_else(|| cwd.clone()));
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
            curio_root: None,
            curio_root_found: false,
            curio_source_ready: false,
            curio_receipts: BTreeSet::new(),
            cwd,
            origin: None,
            state_root,
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
            context_window: None,
            max_output: None,
            env: TurnEnv::default(),
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
    pub(crate) fn tools_mode(&self) -> Mode {
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
        plan::plan_path(&self.state_root(), &self.id)
    }

    /// Resolve old saves without a recorded state root from their original directory.
    pub fn state_root(&self) -> PathBuf {
        self.state_root.clone().unwrap_or_else(|| {
            let start = self.origin.as_ref().unwrap_or(&self.cwd);
            nth_context::main_project_root(start).unwrap_or_else(|| start.clone())
        })
    }

    /// Where this session's orchestration keeps its work items: under the
    /// main checkout, wherever a worktree tool moved the session.
    pub fn work_dir(&self) -> PathBuf {
        work::work_dir(&self.state_root(), &self.id.to_string())
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

    /// Sizes the session to its model from `listing`: the context window
    /// and output limit that size the compaction trigger, whether it takes
    /// images and what it costs, and the same for what its subagents and
    /// its clerk run on. Front-ends call it once the model is final. The
    /// limits stay as they are when the listing does not name the model, so
    /// a resumed session's saved limits still fire its compaction; a caller
    /// switching to another model clears them first.
    pub fn size_to(&mut self, listing: &Listing) {
        let info = listing.get(&self.model);
        if let Some(info) = info {
            self.set_limits(info.context, info.output);
        }
        // Unknown counts as taking images: the providers still turn an image
        // into a note for a model that takes none.
        self.env.takes_images = info.and_then(|info| info.takes_images).unwrap_or(true);
        self.env.cost = info.and_then(|info| info.cost);
        for llm in [&mut self.env.subagent, &mut self.env.clerk]
            .into_iter()
            .flatten()
        {
            *llm = ModelSettings::sized(llm.llm.clone(), listing);
        }
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
}

#[cfg(test)]
pub(crate) mod tests {

    use nth_protocol::{AssistantMessage, Llm, StreamEvent, ToolCall};
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::FrontEnd;

    #[test]
    fn a_fresh_env_takes_the_defaults() {
        let env = TurnEnv::default();

        assert_eq!(env.max_steps, DEFAULT_MAX_STEPS);
        assert_eq!(env.tool_timeout, Some(DEFAULT_TOOL_TIMEOUT));
        assert!(env.takes_images, "unknown counts as taking images");
        assert_eq!(env.compaction, Compaction::default());
        assert_eq!((env.cost, env.images), (None, None));
        assert_eq!((env.subagent, env.clerk), (None, None));
        assert_eq!(Session::new("glm-5.3", ".".into()).env, TurnEnv::default());
    }

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

    fn listed(id: &str, context: Option<u64>, output: Option<u64>) -> nth_protocol::ModelInfo {
        nth_protocol::ModelInfo {
            id: id.into(),
            name: None,
            context,
            output,
            efforts: Vec::new(),
            origin: None,
            cost: None,
            takes_images: None,
        }
    }

    #[test]
    fn size_to_puts_the_listed_window_and_output_on_the_session() {
        let cost = Cost {
            input: 1.0,
            output: 2.0,
            cache_read: None,
            cache_write: None,
        };
        let mut entry = listed("zen/a", Some(200_000), Some(64_000));
        entry.takes_images = Some(false);
        entry.cost = Some(cost);
        let listing = Listing::from(vec![entry]);
        let mut session = Session::new("zen/a", "/repo".into());

        session.size_to(&listing);

        assert_eq!(session.context_window, Some(200_000));
        assert_eq!(session.max_output, Some(64_000));
        assert!(!session.env.takes_images);
        assert_eq!(session.env.cost, Some(cost));
    }

    #[test]
    fn size_to_leaves_an_unlisted_model_unknown() {
        let listing = Listing::from(vec![listed("zen/b", Some(1_000), Some(100))]);
        let mut session = Session::new("zen/a", "/repo".into());

        session.size_to(&listing);

        assert_eq!(session.context_window, None);
        assert_eq!(session.max_output, None);
        assert!(session.env.takes_images, "unknown counts as taking images");
        assert_eq!(session.env.cost, None);
    }

    #[test]
    fn size_to_keeps_known_limits_for_an_unlisted_model() {
        // A resumed session's saved limits still fire its compaction before
        // a listing names its model.
        let listing = Listing::from(vec![listed("zen/b", Some(1_000), Some(100))]);
        let mut resumed = Session::new("zen/a", "/repo".into());
        resumed.set_limits(Some(100_000), Some(8_000));

        resumed.size_to(&listing);

        assert_eq!(resumed.context_window, Some(100_000));
        assert_eq!(resumed.max_output, Some(8_000));
    }

    #[test]
    fn limits_come_from_the_listing_entry_of_the_model() {
        let listing = Listing::from(vec![
            listed("zen/a", Some(200_000), Some(64_000)),
            listed("go/c", Some(1_000_000), None),
        ]);
        let mut session = Session::new("go/c", "/repo".into());
        session.set_limits(Some(1), Some(1));

        session.size_to(&listing);

        // An output limit the entry leaves out is unknown, not the old one.
        assert_eq!(
            (session.context_window, session.max_output),
            (Some(1_000_000), None)
        );
    }

    #[test]
    fn size_to_resizes_the_subagent_and_clerk_llms() {
        let mut images = listed("zen/b", Some(400_000), Some(32_000));
        images.takes_images = Some(true);
        let listing = Listing::from(vec![listed("zen/a", Some(200_000), Some(64_000)), images]);
        let picked = |model: &str| ModelSettings {
            context_window: Some(1),
            max_output: Some(1),
            ..ModelSettings::bare(Llm {
                model: model.into(),
                effort: Effort::Low,
            })
        };
        let mut session = Session::new("zen/a", "/repo".into());
        session.env.subagent = Some(picked("zen/a"));
        session.env.clerk = Some(picked("zen/b"));

        session.size_to(&listing);

        let subagent = session.env.subagent.expect("kept");
        assert_eq!(subagent.llm.model, "zen/a");
        assert_eq!(subagent.llm.effort, Effort::Low);
        assert_eq!(
            (subagent.context_window, subagent.max_output),
            (Some(200_000), Some(64_000))
        );
        let clerk = session.env.clerk.expect("kept");
        assert_eq!(
            (clerk.context_window, clerk.max_output, clerk.takes_images),
            (Some(400_000), Some(32_000), true)
        );
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
        session.env = TurnEnv {
            compaction: Compaction {
                auto: false,
                reserved: 1_000,
                preserve: 2_000,
            },
            max_steps: 3,
            tool_timeout: None,
            takes_images: false,
            cost: Some(Cost {
                input: 1.0,
                output: 2.0,
                cache_read: None,
                cache_write: None,
            }),
            images: Some("/data/images".into()),
            subagent: Some(ModelSettings::default()),
            clerk: Some(ModelSettings::default()),
        };
        session.last_usage = Some(Usage {
            input: 10,
            output: 2,
            ..Usage::default()
        });

        let json = serde_json::to_string(&session).expect("serializes");
        let mut back: Session = serde_json::from_str(&json).expect("deserializes");

        assert_eq!(
            back.env,
            TurnEnv::default(),
            "the config's and the listing's, never the save's"
        );
        back.env = session.env.clone();
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
        assert_eq!(old.env.compaction, Compaction::default());
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
}
