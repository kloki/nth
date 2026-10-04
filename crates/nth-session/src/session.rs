use std::{
    collections::BTreeSet,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::SystemTime,
};

use nth_context::Context;
use nth_protocol::{Asker, Effort, Event, Message, Provider, Tool, ToolContext};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{DEFAULT_MAX_STEPS, Error, Route, run_turn, system_prompt};

/// One conversation: who it runs for, where, and everything said so far.
/// Serializable so the [`Store`](crate::Store) can persist and resume it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Also sent to the provider so it can route and cache per conversation.
    pub id: Uuid,
    pub cwd: PathBuf,
    pub model: String,
    /// Sessions saved before effort existed load with the model's default.
    #[serde(default)]
    pub effort: Effort,
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
    /// Not saved: it is read afresh for the working directory, so a resumed
    /// session sees the instruction files as they are now.
    #[serde(skip)]
    context: Arc<Context>,
}

impl Session {
    pub fn new(model: impl Into<String>, cwd: PathBuf) -> Self {
        let model = model.into();
        let context = Arc::<Context>::default();
        let messages = vec![Message::System(system_prompt(&model, &cwd, &context))];
        let now = SystemTime::now();
        Self {
            id: Uuid::new_v4(),
            cwd,
            model,
            effort: Effort::default(),
            created_at: now,
            updated_at: now,
            messages,
            loaded_instructions: BTreeSet::new(),
            max_steps: DEFAULT_MAX_STEPS,
            context,
        }
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

    pub fn context(&self) -> &Arc<Context> {
        &self.context
    }

    /// Nothing has been asked yet.
    pub fn is_empty(&self) -> bool {
        self.title().is_none()
    }

    /// The first line of the first prompt, which names the session in lists.
    pub fn title(&self) -> Option<&str> {
        self.messages.iter().find_map(|m| match m {
            Message::User(text) => Some(text.trim().lines().next().unwrap_or_default()),
            _ => None,
        })
    }

    /// Later turns go to `model`. The history is kept; only the system
    /// prompt changes, since it names the model.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
        self.rewrite_system_prompt();
    }

    fn rewrite_system_prompt(&mut self) {
        if let Some(first @ Message::System(_)) = self.messages.first_mut() {
            *first = Message::System(system_prompt(&self.model, &self.cwd, &self.context));
        }
    }

    /// Adds a user message and runs the turn it starts, until it ends or
    /// `cancel` interrupts it. Tools ask their questions through `asker`.
    pub async fn prompt(
        &mut self,
        text: impl Into<String>,
        provider: &dyn Provider,
        tools: &[Box<dyn Tool>],
        asker: &Asker,
        events: &mpsc::Sender<Event>,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        self.messages.push(Message::User(text.into()));
        self.updated_at = SystemTime::now();
        // The system prompt's files count as loaded, so read never repeats them.
        let mut loaded = self.loaded_instructions.clone();
        loaded.extend(self.context.instructions.iter().map(|i| i.path.clone()));
        let ctx = ToolContext {
            instructions: Arc::new(Mutex::new(loaded)),
            context: self.context.clone(),
            asker: asker.clone(),
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
        self.loaded_instructions = ctx
            .instructions
            .lock()
            .expect("only poisoned if a holder panicked")
            .clone();
        result
    }
}

fn default_max_steps() -> usize {
    DEFAULT_MAX_STEPS
}

#[cfg(test)]
mod tests {
    use nth_protocol::{AssistantMessage, ToolCall};

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
        assert_eq!(session.title(), Some("fix the build"));
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
                Message::System(system_prompt("kimi-k3", ".".as_ref(), &Context::default())),
                Message::User("go".into()),
            ]
        );
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
            Message::System(system_prompt("kimi-k3", "/repo".as_ref(), &context)),
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
        use nth_protocol::StreamEvent;

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
                &Asker::default(),
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
}
