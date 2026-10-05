use std::time::Duration;

use crate::{ToolCall, ToolResult, Usage};

/// What a running session reports to its front-ends.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStarted(ToolCall),
    /// What a running tool has produced so far, such as a command's output,
    /// in whole lines except for the last chunk. Arrives between the call's
    /// `ToolStarted` and `ToolFinished`.
    ToolOutput {
        call_id: String,
        text: String,
    },
    ToolFinished {
        call: ToolCall,
        result: ToolResult,
    },
    /// After each model reply, when the provider reports it.
    Usage(Usage),
    /// What background monitors said, handed to the model between steps as
    /// a user message.
    Notice(String),
    /// A provider error is being retried: the front-end should show that a
    /// new attempt follows in `delay`, the `attempt`-th so far. The partial
    /// reply for the step is kept.
    Retry {
        attempt: u32,
        delay: Duration,
    },
}
