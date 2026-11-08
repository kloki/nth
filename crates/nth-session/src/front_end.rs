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
    pub asker: Asker,
    pub screen: Screen,
    pub monitors: Monitors,
    /// Read by the turn, never by a tool: the tools that post to it hold
    /// it from construction.
    pub inbox: Inbox,
    pub context_preview: ContextPreview,
}
