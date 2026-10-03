use std::path::PathBuf;

use futures::future::BoxFuture;
use tokio::sync::mpsc;

use crate::Event;

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON schema of the arguments object.
    pub parameters: serde_json::Value,
}

pub struct ToolContext {
    pub cwd: PathBuf,
    /// Where a tool streams what it produces while it runs.
    pub output: OutputSink,
}

impl ToolContext {
    /// A context whose output goes nowhere.
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            cwd,
            output: OutputSink::default(),
        }
    }
}

/// Sends a tool's output as `Event::ToolOutput` for the call it belongs to,
/// so tools never deal in call ids. The default sink drops everything.
#[derive(Debug, Clone, Default)]
pub struct OutputSink(Option<(mpsc::Sender<Event>, String)>);

impl OutputSink {
    pub fn new(events: mpsc::Sender<Event>, call_id: String) -> Self {
        Self(Some((events, call_id)))
    }

    /// Waits while the channel is full, which slows the tool down to what
    /// the front-end can show rather than piling output up in memory.
    pub async fn send(&self, text: String) {
        let Some((events, call_id)) = &self.0 else {
            return;
        };
        let call_id = call_id.clone();
        // No listener is fine: the tool's result does not depend on it.
        let _ = events.send(Event::ToolOutput { call_id, text }).await;
    }
}

/// `Err` is not a failure of nth: its text goes back to the model, which
/// reads it and tries again.
pub type ToolResult = Result<String, String>;

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult>;
}
