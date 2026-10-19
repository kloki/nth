use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use futures::future::BoxFuture;
use nth_context::Context;
use tokio::sync::mpsc;

use crate::{
    Asker, Attachments, Compaction, Event, Inbox, Llm, Monitors, Screen, SubagentLlm, Workdir,
    Writable,
};

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: String,
    /// JSON schema of the arguments object.
    pub parameters: serde_json::Value,
}

pub struct ToolContext {
    pub cwd: PathBuf,
    /// Where a tool moves the session's working directory, as entering a
    /// worktree does; later steps of the turn run there.
    pub workdir: Workdir,
    /// Working directories added with `/add-dir`, for a tool that starts
    /// another session in the same ones.
    pub extra_dirs: Vec<PathBuf>,
    /// Where a tool streams what it produces while it runs.
    pub output: OutputSink,
    /// Instruction files already in the conversation. Shared by the calls
    /// of a turn, which run in parallel, so each file is attached once.
    pub instructions: LoadedInstructions,
    /// What the session knows about its project, such as its skills.
    pub context: Arc<Context>,
    /// Where a tool asks you questions while it runs.
    pub asker: Asker,
    /// Where a tool switches what the content panel shows.
    pub screen: Screen,
    /// The commands left running in the background.
    pub monitors: Monitors,
    /// Where what the background says waits for the model.
    pub inbox: Inbox,
    /// The files the tools may write: all of them, or in plan mode only
    /// the plan file.
    pub writable: Writable,
    /// The model and effort the session runs on, for a tool that starts
    /// another session on the same.
    pub llm: Llm,
    /// What a subagent the session starts runs on, when that is not the
    /// session's own model; an agent's own model still wins.
    pub subagent: Option<SubagentLlm>,
    /// What the built-in clerk runs on, when that is not `subagent`'s.
    pub clerk: Option<SubagentLlm>,
    /// The model's context window and output limit, when the front-end
    /// knows them, for a tool that starts another session on the same
    /// model: they size the child's compaction trigger.
    pub context_window: Option<u64>,
    pub max_output: Option<u64>,
    /// How the session keeps itself inside the window, for a tool that
    /// starts another session on the same model: a child of a session with
    /// automatic compaction off must not compact on its own.
    pub compaction: Compaction,
    /// The id of the session the call runs for, for a tool that keeps
    /// something per session; empty outside one.
    pub session: String,
    /// Where a tool puts the images it shows the model, beside its text
    /// result; each call has its own.
    pub attachments: Attachments,
    /// Whether the model takes images, so a tool can refuse to read one
    /// rather than send what the model cannot see.
    pub takes_images: bool,
    /// The folder a tool keeps image files in: the session's, which its
    /// subagents share, so deleting the session deletes them. A tool that
    /// shows images refuses without one, rather than leave them where
    /// another user could read or swap them.
    pub images: Option<PathBuf>,
}

/// How tools reach the person at the front-end: to ask them questions, to
/// switch what they see, to leave commands running, and to leave the model
/// a notice it is woken for. The default reaches nobody, as in a headless
/// run.
#[derive(Debug, Clone, Default)]
pub struct FrontEnd {
    pub asker: Asker,
    pub screen: Screen,
    pub monitors: Monitors,
    pub inbox: Inbox,
}

pub type LoadedInstructions = Arc<Mutex<BTreeSet<PathBuf>>>;

impl ToolContext {
    /// A context whose output goes nowhere, that has loaded nothing and has
    /// nobody to ask or show anything to, no monitors and no inbox.
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            workdir: Workdir::default(),
            extra_dirs: Vec::new(),
            output: OutputSink::default(),
            instructions: LoadedInstructions::default(),
            context: Arc::default(),
            asker: Asker::default(),
            screen: Screen::default(),
            monitors: Monitors::default(),
            inbox: Inbox::default(),
            writable: Writable::Any,
            llm: Llm::default(),
            subagent: None,
            clerk: None,
            context_window: None,
            max_output: None,
            compaction: Compaction::default(),
            session: String::new(),
            attachments: Attachments::default(),
            takes_images: true,
            images: None,
        }
    }

    /// The context one call of a turn runs in: where an earlier step moved
    /// the session, with its questions and `output` tagged for `call_id`
    /// and attachments of its own; the rest is shared with the turn.
    pub fn for_call(&self, call_id: String, output: OutputSink) -> Self {
        Self {
            cwd: self.workdir.moved_to().unwrap_or_else(|| self.cwd.clone()),
            workdir: self.workdir.clone(),
            extra_dirs: self.extra_dirs.clone(),
            output,
            instructions: self.instructions.clone(),
            context: self.context.clone(),
            asker: self.asker.for_call(call_id),
            screen: self.screen.clone(),
            monitors: self.monitors.clone(),
            inbox: self.inbox.clone(),
            writable: self.writable.clone(),
            llm: self.llm.clone(),
            subagent: self.subagent.clone(),
            clerk: self.clerk.clone(),
            context_window: self.context_window,
            max_output: self.max_output,
            compaction: self.compaction,
            session: self.session.clone(),
            attachments: Attachments::default(),
            takes_images: self.takes_images,
            images: self.images.clone(),
        }
    }
}

/// Sends a tool's output as `Event::ToolOutput` for the call it belongs to,
/// so tools never deal in call ids. The default sink drops everything.
#[derive(Debug, Clone, Default)]
pub struct OutputSink(Option<(mpsc::Sender<Event>, String)>);

impl OutputSink {
    pub fn new(events: mpsc::Sender<Event>, call_id: String) -> Self {
        Self(Some((events, call_id)))
    }

    /// Waits while the channel is full, which slows the tool down to what
    /// the front-end can show rather than piling output up in memory.
    pub async fn send(&self, text: String) {
        let Some((events, call_id)) = &self.0 else {
            return;
        };
        let call_id = call_id.clone();
        // No listener is fine: the tool's result does not depend on it.
        let _ = events.send(Event::ToolOutput { call_id, text }).await;
    }
}

/// `Err` is not a failure of nth: its text goes back to the model, which
/// reads it and tries again.
pub type ToolResult = Result<String, String>;

/// A tool's arguments as the struct it expects, or what was wrong with
/// them for the model.
pub fn parse_args<T: serde::de::DeserializeOwned>(args: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))
}

/// Why a tool the user cut short failed; the model reads it after `Error: `.
pub const INTERRUPTED: &str = "interrupted by the user";

/// The text the model reads, and the session saves, before why a call failed.
const FAILED: &str = "Error: ";

/// A tool result's content when the tool failed for `reason`: the shape
/// [`failure_reason`] reads the reason back out of.
pub fn failed(reason: &str) -> String {
    format!("{FAILED}{reason}")
}

/// Why the call failed, when `content` is a result [`failed`] wrote.
pub fn failure_reason(content: &str) -> Option<&str> {
    content.strip_prefix(FAILED)
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult>;
}

/// A shared tool is a tool, so one list can be handed to several sessions.
impl<T: Tool + ?Sized> Tool for Arc<T> {
    fn spec(&self) -> ToolSpec {
        (**self).spec()
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        (**self).call(args, ctx)
    }
}
