use crate::{ToolCall, ToolResult};

/// What a running session reports to its front-ends.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStarted(ToolCall),
    ToolFinished { call: ToolCall, result: ToolResult },
}
