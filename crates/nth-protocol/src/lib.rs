//! Types every nth crate shares: messages, events and the traits that let a
//! provider or tool be swapped without touching the session loop.

mod compaction;
mod event;
mod inbox;
mod message;
mod mode;
mod monitor;
mod provider;
mod question;
mod screen;
mod tool;
mod workdir;
mod writable;

pub use compaction::Compaction;
pub use event::{Event, retry_label};
pub use inbox::{
    Inbox, NOTICE_LINES, NoticeSummary, TaskId, TaskNotice, TaskOutcome, Todo, WorkNotice,
    WorkOutcome, WorkState, one_line, quiet_text, split_notices, todo_counts, todo_lines,
};
pub use message::{AssistantMessage, Message, ToolCall};
pub use mode::Mode;
pub use monitor::{
    MonitorEnd, MonitorEvent, MonitorId, Monitors, Registered, StoppedBy, Stream,
    log_dir as monitor_log_dir,
};
pub use provider::{
    BoxError, ContextOverflow, Cost, Effort, Failed, Listing, Llm, ModelInfo, Origin, Provider,
    Request, Retry, StreamEvent, Usage,
};
pub use question::{Answer, Ask, Asker, Question, QuestionOption, Reply};
pub use screen::{Panel, Screen, Show};
pub use tool::{FrontEnd, LoadedInstructions, OutputSink, Tool, ToolContext, ToolResult, ToolSpec};
pub use workdir::Workdir;
pub use writable::Writable;
