//! Types every nth crate shares: messages, events and the traits that let a
//! provider or tool be swapped without touching the session loop.

mod event;
mod message;
mod provider;
mod question;
mod screen;
mod tool;

pub use event::Event;
pub use message::{AssistantMessage, Message, ToolCall};
pub use provider::{BoxError, Effort, ModelInfo, Provider, Request, StreamEvent, Usage};
pub use question::{Answer, Ask, Asker, Question, QuestionOption, Reply};
pub use screen::{Panel, Screen};
pub use tool::{FrontEnd, LoadedInstructions, OutputSink, Tool, ToolContext, ToolResult, ToolSpec};
