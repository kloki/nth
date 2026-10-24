use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Anchor {
    pub id: String,
    pub label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TargetKind {
    Element,
    Region,
    Document,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Target {
    #[serde(default)]
    pub element_id: Option<String>,
    pub kind: TargetKind,
    #[serde(default)]
    pub rect: Option<Rect>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub quote: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Open,
    Addressed,
    Resolved,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Saved,
    Queued,
    Recorded,
    Answered,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Author {
    Human,
    Agent,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub author: Author,
    pub body: String,
    pub created_at: u64,
    pub delivery: Delivery,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Thread {
    pub id: String,
    pub number: u64,
    pub version: u64,
    pub created_revision: u64,
    pub target: Target,
    pub attachment: String,
    pub status: Status,
    pub messages: Vec<Message>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Revision {
    pub number: u64,
    pub title: String,
    pub hash: String,
    pub anchors: Vec<Anchor>,
    pub html_url: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Curio {
    pub id: String,
    pub title: String,
    pub session_id: String,
    pub current_revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub curio: Curio,
    pub revision: Revision,
    pub threads: Vec<Thread>,
    pub revisions: Vec<Revision>,
    pub state_version: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Batch {
    pub id: String,
    pub session_id: String,
    pub curio_id: String,
    /// Current revision at submission time; messages retain their reviewed context.
    pub revision: u64,
    pub messages: Vec<SubmittedMessage>,
    pub recorded: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubmittedMessage {
    pub thread_id: String,
    pub thread_version: u64,
    pub message: Message,
    pub target: Target,
    /// Revision viewed when this human message was written. None for legacy data.
    #[serde(default)]
    pub reviewed_revision: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Publish {
    pub request_id: String,
    pub curio_id: Option<String>,
    pub expected_revision: Option<u64>,
    pub title: Option<String>,
    pub html: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Published {
    pub curio: Curio,
    pub revision: Revision,
    pub detached_thread_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Comment {
    pub request_id: String,
    pub revision: u64,
    pub target: Target,
    pub body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Reply {
    pub request_id: String,
    pub expected_version: u64,
    pub body: String,
    #[serde(default)]
    pub mark_addressed: bool,
    #[serde(default)]
    pub revision: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Send {
    pub request_id: String,
    pub message_ids: Vec<String>,
}
