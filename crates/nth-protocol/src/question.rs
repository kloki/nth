use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};

/// One question the model asks you, as the question tool takes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Question {
    pub question: String,
    /// A short label, shown on the question's tab.
    pub header: String,
    /// Any number of options may be picked, rather than one.
    #[serde(default)]
    pub multiple: bool,
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub label: String,
    /// One line on what the option means or costs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Several lines shown as written while the option is highlighted,
    /// such as an ASCII mockup or a code snippet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

/// Your answer to one question: the labels you picked, and what you typed
/// in the open field, if anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answer {
    pub picked: Vec<String>,
    pub typed: Option<String>,
}

/// What a front-end sends back for an [`Ask`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// One answer per question, in the order they were asked.
    Answered(Vec<Answer>),
    Declined,
}

/// Questions waiting on you, for the front-end to show. Dropping `reply`
/// unanswered counts as nobody being there to ask.
#[derive(Debug)]
pub struct Ask {
    pub call_id: String,
    pub questions: Vec<Question>,
    pub reply: oneshot::Sender<Reply>,
}

/// Lets a tool ask you questions through the front-end, for the call it
/// belongs to, as [`OutputSink`](crate::OutputSink) does for output. The
/// default asker has nobody to ask, as in a headless run.
#[derive(Debug, Clone, Default)]
pub struct Asker {
    front_end: Option<mpsc::Sender<Ask>>,
    call_id: String,
}

impl Asker {
    pub fn new(front_end: mpsc::Sender<Ask>) -> Self {
        Self {
            front_end: Some(front_end),
            call_id: String::new(),
        }
    }

    /// The same asker, sending its questions under `call_id`.
    pub fn for_call(&self, call_id: String) -> Self {
        Self {
            front_end: self.front_end.clone(),
            call_id,
        }
    }

    /// Waits for your reply; `None` when there is nobody to ask.
    pub async fn ask(&self, questions: Vec<Question>) -> Option<Reply> {
        let front_end = self.front_end.as_ref()?;
        let (reply, answer) = oneshot::channel();
        let ask = Ask {
            call_id: self.call_id.clone(),
            questions,
            reply,
        };
        front_end.send(ask).await.ok()?;
        answer.await.ok()
    }
}
