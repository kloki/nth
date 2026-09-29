use std::{path::PathBuf, time::SystemTime};

use nth_protocol::{Effort, Event, Message, Provider, Tool, ToolContext};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{Error, Route, run_turn, system_prompt};

/// One conversation: who it runs for, where, and everything said so far.
/// Serializable so a later store can persist and resume it.
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
    pub messages: Vec<Message>,
}

impl Session {
    pub fn new(model: impl Into<String>, cwd: PathBuf) -> Self {
        let model = model.into();
        let messages = vec![Message::System(system_prompt(&model, &cwd))];
        Self {
            id: Uuid::new_v4(),
            cwd,
            model,
            effort: Effort::default(),
            created_at: SystemTime::now(),
            messages,
        }
    }

    /// Later turns go to `model`. The history is kept; only the system
    /// prompt changes, since it names the model.
    pub fn set_model(&mut self, model: impl Into<String>) {
        self.model = model.into();
        if let Some(first @ Message::System(_)) = self.messages.first_mut() {
            *first = Message::System(system_prompt(&self.model, &self.cwd));
        }
    }

    /// Adds a user message and runs the turn it starts, until it ends or
    /// `cancel` interrupts it.
    pub async fn prompt(
        &mut self,
        text: impl Into<String>,
        provider: &dyn Provider,
        tools: &[Box<dyn Tool>],
        events: &mpsc::Sender<Event>,
        cancel: &CancellationToken,
    ) -> Result<(), Error> {
        self.messages.push(Message::User(text.into()));
        let ctx = ToolContext {
            cwd: self.cwd.clone(),
        };
        run_turn(
            provider,
            Route {
                model: &self.model,
                effort: self.effort,
                session_id: &self.id.to_string(),
            },
            tools,
            &ctx,
            &mut self.messages,
            events,
            cancel,
        )
        .await
    }
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
        assert_ne!(a.id, b.id);
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
                Message::System(system_prompt("kimi-k3", ".".as_ref())),
                Message::User("go".into()),
            ]
        );
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
