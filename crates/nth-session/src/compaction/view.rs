//! What a request carries of a compacted history.

use std::borrow::Cow;

use nth_protocol::Message;

use super::{checkpoint, last_marker};

/// The messages a request carries. Without a compaction marker, the history
/// is the request. With one, the request starts at the last marker, read
/// the way opencode reads its checkpoint: the system prompts, then one user
/// message holding the summary and the text kept beside it, then everything
/// after the marker verbatim. The request so never ends on the summary.
/// Only this filter and the check on what a compaction left ever see the
/// marker; the protocols never do.
pub(crate) fn effective(messages: &[Message]) -> Cow<'_, [Message]> {
    let Some(at) = last_marker(messages) else {
        return Cow::Borrowed(messages);
    };
    let mut sent: Vec<Message> = messages[..at]
        .iter()
        .filter(|message| matches!(message, Message::System(_)))
        .cloned()
        .collect();
    if let Message::Compaction { summary, recent } = &messages[at] {
        sent.push(Message::User(checkpoint(summary, recent)));
    }
    sent.extend(messages[at + 1..].iter().cloned());
    Cow::Owned(sent)
}
