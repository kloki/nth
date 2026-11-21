//! What a turn reaches the person at the front-end through.

use nth_protocol::{Asker, ContextPreview, Monitors, Screen};

use crate::Inbox;

/// How a turn reaches the person at the front-end: tools ask them
/// questions, switch what they see and leave commands running through the
/// handles it passes on to every call; the model hears what waits in the
/// inbox between its steps; and the turn publishes its context preview.
/// The default reaches nobody, as in a headless run.
#[derive(Debug, Clone, Default)]
pub struct FrontEnd {
    pub wakeup: nth_protocol::Wakeup,
    pub asker: Asker,
    pub screen: Screen,
    pub monitors: Monitors,
    /// Read by the turn, never by a tool: the tools that post to it hold
    /// it from construction.
    pub inbox: Inbox,
    pub context_preview: ContextPreview,
    /// Original human input and destination for a title job, supplied only
    /// by an owner that will settle the job and its usage.
    pub title_prompt: Option<String>,
    pub title_jobs: Option<tokio::sync::mpsc::UnboundedSender<crate::title::Job>>,
    pub title_store: Option<crate::Store>,
}
