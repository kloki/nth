//! Types every nth crate shares: messages, events and the traits that let a
//! provider or tool be swapped without touching the session loop.

mod event;
mod message;
mod provider;
mod tool;

pub use event::Event;
pub use message::{AssistantMessage, Message, ToolCall};
pub use provider::{BoxError, Effort, ModelInfo, Provider, Request, StreamEvent, Usage};
pub use tool::{LoadedInstructions, OutputSink, Tool, ToolContext, ToolResult, ToolSpec};
