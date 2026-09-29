use std::{path::PathBuf, time::SystemTime};

use nth_protocol::{Event, Message, Provider, Tool, ToolContext};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::{Error, run_turn, system_prompt};

/// One conversation: who it runs for, where, and everything said so far.
/// Serializable so a later store can persist and resume it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Also sent to the provider so it can route and cache per conversation.
    pub id: Uuid,
    pub cwd: PathBuf,
    pub model: String,
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
            created_at: SystemTime::now(),
            messages,
        }
    }

    /// Adds a user message and runs the turn it starts.
    pub async fn prompt(
        &mut self,
        text: impl Into<String>,
        provider: &dyn Provider,
        tools: &[Box<dyn Tool>],
        events: &mpsc::Sender<Event>,
    ) -> Result<(), Error> {
        self.messages.push(Message::User(text.into()));
        let ctx = ToolContext {
            cwd: self.cwd.clone(),
        };
        run_turn(provider, tools, &ctx, &mut self.messages, events).await
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
