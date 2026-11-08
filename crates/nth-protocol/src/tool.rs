use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::future::BoxFuture;
use nth_context::{Agent, Context, instructions::Watch};
use tokio::sync::mpsc;

use crate::{
    Asker, Attachments, Compaction, Event, ModelSettings, Monitors, Screen, Workdir, Writable,
};

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: String,
    /// JSON schema of the arguments object.
    pub parameters: serde_json::Value,
}

/// Cloned whole into every call by [`ToolContext::for_call`], so a field
/// is shared with the turn unless that says otherwise.
#[derive(Clone)]
pub struct ToolContext {
    /// Latest committed main-session preview, shared by all calls of a turn.
    pub context_preview: crate::ContextPreview,
    pub is_subagent: bool,
    pub cwd: PathBuf,
    /// Stable root for this session's repository-local generated state.
    pub state_root: Option<PathBuf>,
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
    /// What the model last saw of the instruction files, so a change
    /// reaches it at the next step.
    pub watch: Watch,
    /// What the session knows about its project, such as its skills.
    pub context: Arc<Context>,
    /// Where a tool asks you questions while it runs.
    pub asker: Asker,
    /// Where a tool switches what the content panel shows.
    pub screen: Screen,
    /// The commands left running in the background.
    pub monitors: Monitors,
    /// The files the tools may write: all of them, or in plan mode only
    /// the plan file.
    pub writable: Writable,
    /// The primary agent the session runs as, in place of plan or act:
    /// the model is offered only the tools it allows.
    pub agent: Option<Agent>,
    /// What a tool that starts another session starts it on.
    pub child: ChildLlm,
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
    /// How long a call may run before the session loop stops it, for a
    /// tool not in `tool_names::UNTIMED`; `None` lets every call run.
    pub tool_timeout: Option<Duration>,
}

pub type LoadedInstructions = Arc<Mutex<BTreeSet<PathBuf>>>;

impl ToolContext {
    /// A context whose output goes nowhere, that has loaded nothing and has
    /// nobody to ask or show anything to and no monitors.
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            context_preview: crate::ContextPreview::default(),
            cwd,
            state_root: None,
            is_subagent: false,
            workdir: Workdir::default(),
            extra_dirs: Vec::new(),
            output: OutputSink::default(),
            instructions: LoadedInstructions::default(),
            watch: Watch::default(),
            context: Arc::default(),
            asker: Asker::default(),
            screen: Screen::default(),
            monitors: Monitors::default(),
            writable: Writable::Any,
            agent: None,
            child: ChildLlm::default(),
            session: String::new(),
            attachments: Attachments::default(),
            takes_images: true,
            images: None,
            tool_timeout: None,
        }
    }

    /// The context one call of a turn runs in: where an earlier step moved
    /// the session, with its questions and `output` tagged for `call_id`
    /// and attachments of its own; the rest is shared with the turn.
    pub fn for_call(&self, call_id: String, output: OutputSink) -> Self {
        Self {
            cwd: self.workdir.moved_to().unwrap_or_else(|| self.cwd.clone()),
            asker: self.asker.for_call(call_id),
            output,
            attachments: Attachments::default(),
            ..self.clone()
        }
    }
}

/// What a tool that starts another session, as the task tool does, starts
/// it on: the session's own model and limits, or what the front-end picked
/// for its subagents or its clerk, and how it keeps inside the window,
/// since a child of a session with automatic compaction off must not
/// compact on its own.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ChildLlm {
    pub own: ModelSettings,
    /// What a subagent runs on, when that is not `own`; an agent's own
    /// model still wins.
    pub subagent: Option<ModelSettings>,
    /// What the built-in clerk runs on, when that is not `subagent`'s.
    pub clerk: Option<ModelSettings>,
    pub compaction: Compaction,
}

impl ChildLlm {
    /// What a subagent runs on before its agent's own model is considered:
    /// for the clerk its pick, else the subagents', else the session's own.
    pub fn for_agent(&self, clerk: bool) -> &ModelSettings {
        let picked = match clerk {
            true => self.clerk.as_ref().or(self.subagent.as_ref()),
            false => self.subagent.as_ref(),
        };
        picked.unwrap_or(&self.own)
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

/// The timeout arguments tools used to take, refused with the one they take now.
pub fn reject_old_timeout(args: &serde_json::Value) -> Result<(), String> {
    for key in ["timeout", "timeout_secs"] {
        if args.get(key).is_some() {
            return Err(format!("obsolete argument {key}; use timeout_ms"));
        }
    }
    Ok(())
}

/// Why a tool the user cut short failed; the model reads it after `Error: `.
pub const INTERRUPTED: &str = "interrupted by the user";

/// Why a tool the session loop stopped after `ceiling` failed; the model
/// reads it after `Error: `.
pub fn timed_out(ceiling: Duration) -> String {
    format!("timed out after {ceiling:?}; the tool was stopped")
}

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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{Ask, Image};

    fn image(sha256: &str) -> Image {
        Image {
            mime: "image/png".into(),
            sha256: sha256.into(),
            width: 1,
            height: 1,
            path: format!("/data/{sha256}.png").into(),
            data: None,
        }
    }

    #[tokio::test]
    async fn for_call_shares_the_turn_and_owns_its_attachments_and_asker() {
        let (asks_tx, mut asks) = mpsc::channel::<Ask>(2);
        let child = ChildLlm {
            own: ModelSettings::bare(crate::Llm {
                model: "zen/a".into(),
                effort: crate::Effort::High,
            }),
            ..ChildLlm::default()
        };
        let turn = ToolContext {
            asker: Asker::new(asks_tx),
            is_subagent: true,
            state_root: Some("/repo".into()),
            extra_dirs: vec!["/other".into()],
            writable: Writable::Plan {
                file: "/repo/plan.md".into(),
                memory: None,
                source: None,
            },
            child: child.clone(),
            session: "s1".into(),
            takes_images: false,
            images: Some("/data/images/s1".into()),
            tool_timeout: Some(Duration::from_secs(9)),
            ..ToolContext::new("/repo".into())
        };
        turn.workdir
            .enter(Path::new("/repo"), "/repo/.nth/worktrees/x".into());

        let one = turn.for_call("1".into(), OutputSink::default());
        let two = turn.for_call("2".into(), OutputSink::default());
        one.attachments.add(image("a"));

        assert_eq!(one.attachments.take().len(), 1);
        assert!(two.attachments.take().is_empty(), "calls never mix images");
        assert!(turn.attachments.take().is_empty());
        assert_eq!(
            one.cwd,
            Path::new("/repo/.nth/worktrees/x"),
            "where an earlier step moved the session"
        );
        one.instructions
            .lock()
            .expect("not poisoned")
            .insert("/repo/AGENTS.md".into());
        assert_eq!(
            two.instructions.lock().expect("not poisoned").len(),
            1,
            "loaded instructions are the turn's"
        );

        // Nobody answers: the ask only has to reach the front-end tagged.
        let asking = two.asker.ask(Vec::new());
        let (_, ask) = tokio::join!(asking, async {
            let ask = asks.recv().await.expect("the ask reached the front-end");
            let call_id = ask.call_id.clone();
            drop(ask);
            call_id
        });
        assert_eq!(ask, "2");

        // Everything else is the turn's, a field added later included.
        assert!(one.is_subagent);
        assert_eq!(one.state_root, turn.state_root);
        assert_eq!(one.extra_dirs, turn.extra_dirs);
        assert_eq!(one.writable, turn.writable);
        assert_eq!(one.child, child);
        assert_eq!(one.session, "s1");
        assert!(!one.takes_images);
        assert_eq!(one.images, turn.images);
        assert_eq!(one.tool_timeout, turn.tool_timeout);
    }
}
