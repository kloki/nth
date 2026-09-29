use futures::{future::BoxFuture, stream::BoxStream};
use serde::Serialize;

use crate::{Message, ToolCall, ToolSpec};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub struct Request<'a> {
    /// Per request rather than per provider, so a session can switch models
    /// without rebuilding its client.
    pub model: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    /// Emitted once the call's arguments are complete.
    ToolCall(ToolCall),
}

/// A model a provider can serve, with limits when they are known.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: Option<String>,
    /// Context window in tokens.
    pub context: Option<u64>,
    /// Maximum output tokens.
    pub output: Option<u64>,
}

pub trait Provider: Send + Sync {
    /// Only models this provider can actually talk to.
    fn models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, BoxError>>;

    fn stream<'a>(
        &'a self,
        request: Request<'a>,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>;
}
