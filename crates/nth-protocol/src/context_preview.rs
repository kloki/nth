//! The logical next request as nth-session projects it, and the shared
//! slot holding its latest committed snapshot for tools and front-ends.
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::{Effort, Message, Mode, Usage};

/// One published snapshot of the session preview.
pub type Snapshot = Arc<Preview>;

/// Where in a turn a snapshot was taken. Only committed boundaries are
/// published; partial work (a streaming reply, a running tool) never is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// No turn runs: what the next prompt would go out with.
    Idle,
    /// A request is about to go out with exactly these entries.
    PendingReply,
    /// The reply is in and its tool calls are about to run.
    PendingResults,
    /// The results and the between-steps hand-over are in the history.
    Ready,
    /// The window is full and a compaction runs before the next request.
    CompactionRequired,
}

impl Phase {
    /// The text a front-end shows for it.
    pub fn label(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::PendingReply => "pending reply",
            Phase::PendingResults => "pending results",
            Phase::Ready => "ready",
            Phase::CompactionRequired => "compaction required",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    pub estimated_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub message: Message,
    pub estimated_tokens: u64,
    pub checkpoint: bool,
    /// Availability reflects whether bytes are loaded, not whether the path exists.
    pub images_available: Vec<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub system: u64,
    pub conversation: u64,
    pub tools: u64,
    pub images: u64,
    pub input: u64,
    pub context_window: Option<u64>,
    pub output_reserve: u64,
    pub usable: Option<u64>,
    pub available: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    pub phase: Phase,
    pub model: String,
    pub effort: Effort,
    pub mode: Mode,
    pub agent: Option<String>,
    pub entries: Vec<Entry>,
    pub tools: Vec<ToolDefinition>,
    pub budget: Budget,
    pub last_usage: Option<Usage>,
    pub compaction_required: bool,
    /// Submitted but not yet delivered items, in inbox order.
    pub pending: Vec<(String, String)>,
}

/// The newest committed snapshot nth-session published. Clones share one
/// slot; a reader that falls behind skips to the newest.
#[derive(Debug, Clone)]
pub struct ContextPreview {
    slot: watch::Sender<Option<Snapshot>>,
    /// Something asked for a snapshot without subscribing: the `context` tool.
    wanted: Arc<AtomicBool>,
}

/// No receiver is kept, so a headless context builds nothing until
/// something subscribes or asks.
impl Default for ContextPreview {
    fn default() -> Self {
        let (slot, _) = watch::channel(None);
        Self {
            slot,
            wanted: Arc::default(),
        }
    }
}

impl ContextPreview {
    pub fn get(&self) -> Option<Snapshot> {
        self.slot.borrow().clone()
    }

    pub fn publish(&self, snapshot: Snapshot) {
        self.slot.send_replace(Some(snapshot));
    }

    /// A reader of every later snapshot. Building one costs O(history), so
    /// a front-end holds this only while it shows them, never between
    /// turns.
    pub fn subscribe(&self) -> watch::Receiver<Option<Snapshot>> {
        self.slot.subscribe()
    }

    /// Asks for snapshots for the rest of the turn without subscribing,
    /// for a reader that polls with [`ContextPreview::get`].
    pub fn want(&self) {
        self.wanted.store(true, Ordering::Relaxed);
    }

    /// Ends what [`ContextPreview::want`] asked for. A front-end shares one
    /// slot across turns, so a `want` that outlived its turn would keep
    /// every later turn building for nobody. With no subscriber the
    /// snapshot goes too: nothing updates it until something asks again,
    /// and a later turn's first look would take it for its own.
    pub fn turn_ended(&self) {
        self.wanted.store(false, Ordering::Relaxed);
        if self.slot.receiver_count() == 0 {
            self.slot.send_replace(None);
        }
    }

    /// Whether a snapshot would reach anyone; the loop builds none otherwise.
    pub fn builds(&self) -> bool {
        self.slot.receiver_count() > 0 || self.wanted.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(phase: Phase) -> Preview {
        Preview {
            phase,
            model: "model".into(),
            effort: Effort::default(),
            mode: Mode::Act,
            agent: None,
            entries: Vec::new(),
            tools: Vec::new(),
            budget: Budget::default(),
            last_usage: None,
            compaction_required: false,
            pending: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_slow_subscriber_sees_the_newest_snapshot() {
        let slot = ContextPreview::default();
        let mut rx = slot.subscribe();
        slot.publish(Arc::new(preview(Phase::PendingReply)));
        slot.publish(Arc::new(preview(Phase::Ready)));
        rx.changed().await.expect("the slot is open");
        assert_eq!(
            rx.borrow_and_update().as_deref().map(|p| p.phase),
            Some(Phase::Ready)
        );
        assert!(!rx.has_changed().expect("the slot is open"));
    }

    #[test]
    fn builds_only_for_a_subscriber_or_a_wanter() {
        let slot = ContextPreview::default();
        assert!(!slot.builds());
        let rx = slot.subscribe();
        assert!(slot.clone().builds(), "clones share the slot");
        drop(rx);
        assert!(!slot.builds());
        slot.clone().want();
        assert!(slot.builds());
        slot.publish(Arc::new(preview(Phase::Idle)));
        slot.clone().turn_ended();
        assert!(!slot.builds(), "a want lasts one turn");
        assert!(
            slot.get().is_none(),
            "unwatched, a later turn would read it"
        );
        let _rx = slot.subscribe();
        slot.publish(Arc::new(preview(Phase::Idle)));
        slot.turn_ended();
        assert!(slot.get().is_some(), "a subscriber still shows it");
    }

    #[test]
    fn phases_keep_their_wire_names() {
        assert_eq!(
            serde_json::to_value(Phase::CompactionRequired).expect("serializes"),
            "compaction_required"
        );
        assert_eq!(Phase::PendingResults.label(), "pending results");
    }
}
