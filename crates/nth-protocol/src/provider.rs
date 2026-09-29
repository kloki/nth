use futures::{future::BoxFuture, stream::BoxStream};

use crate::{Message, ToolCall, ToolSpec};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub struct Request<'a> {
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

pub trait Provider: Send + Sync {
    fn model(&self) -> &str;

    fn stream<'a>(
        &'a self,
        request: Request<'a>,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>;
}
