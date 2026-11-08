//! Types every nth crate shares: messages, events and the traits that let a
//! provider or tool be swapped without touching the session loop.

mod compaction;
mod context_preview;
mod curios;
mod event;
mod image;
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
pub use context_preview::{
    Budget, ContextPreview, Entry, Phase, Preview, Snapshot as ContextSnapshot, ToolDefinition,
};
pub use curios::{CurioDelivery, CurioNotice, Curios, nth_curios};
pub use event::{Event, retry_label};
pub use image::{
    Attachments, Image, MAX_IMAGE_BYTES, REMOVED as IMAGE_REMOVED, too_large as image_too_large,
    unsupported as image_unsupported,
};
pub use inbox::{
    Inbox, NOTICE_LINE_CHARS, NOTICE_LINES, NoticeSummary, TaskId, TaskNotice, TaskOutcome,
    TaskStall, Todo, WorkNotice, WorkOutcome, WorkState, instructions_notice, one_line, quiet_text,
    split_notices, todo_counts, todo_lines,
};
pub use message::{AssistantMessage, Message, ToolCall};
pub use mode::Mode;
pub use monitor::{MonitorEnd, MonitorEvent, MonitorId, Monitors, Registered, StoppedBy, Stream};
pub use nth_context::agents::tool_names;
pub use provider::{
    BoxError, ContextOverflow, Cost, Effort, Failed, Listing, Llm, ModelInfo, Origin, Provider,
    Request, Retry, StreamEvent, SubagentLlm, Usage,
};
pub use question::{Answer, Ask, Asker, Question, QuestionOption, Reply};
pub use screen::{Panel, Screen, Show};
pub use tool::{
    FrontEnd, INTERRUPTED, LoadedInstructions, OutputSink, Tool, ToolContext, ToolResult, ToolSpec,
    failed, failure_reason, parse_args, reject_old_timeout, timed_out,
};
pub use workdir::Workdir;
pub use writable::Writable;
