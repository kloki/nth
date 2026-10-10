use std::{path::PathBuf, time::Duration};

use crate::{ToolCall, ToolResult, Usage};

/// What a running session reports to its front-ends.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    TextDelta(String),
    ReasoningDelta(String),
    ToolStarted(ToolCall),
    /// What a running tool has produced so far, such as a command's output,
    /// in whole lines except for the last chunk. Arrives between the call's
    /// `ToolStarted` and `ToolFinished`.
    ToolOutput {
        call_id: String,
        text: String,
    },
    ToolFinished {
        call: ToolCall,
        result: ToolResult,
    },
    /// After each model reply, when the provider reports it.
    Usage(Usage),
    /// Handed to the model between steps as a user message: what monitors
    /// and subagents said, or a prompt sent mid-turn.
    Notice(String),
    /// A provider error is being retried: the front-end should show that a
    /// new attempt follows in `delay`, the `attempt`-th so far. Text already
    /// streamed for the step stays on screen but is not saved; the retry
    /// starts the reply over.
    Retry {
        attempt: u32,
        delay: Duration,
    },
    /// A tool moved the session's working directory, as entering a
    /// worktree does; the turn's later steps run in it. Comes before that
    /// call's `ToolFinished`.
    Moved(PathBuf),
}

/// How a retry reads, the same in every front-end: whole seconds rounded
/// up, so a sub-second wait never shows as `0s`.
pub fn retry_label(attempt: u32, delay: Duration) -> String {
    let seconds = delay.as_millis().div_ceil(1000);
    format!("retrying in {seconds}s · attempt {attempt}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retry_label_rounds_the_wait_up() {
        assert_eq!(
            retry_label(1, Duration::from_millis(500)),
            "retrying in 1s · attempt 1"
        );
        assert_eq!(
            retry_label(2, Duration::from_millis(1500)),
            "retrying in 2s · attempt 2"
        );
        assert_eq!(
            retry_label(3, Duration::from_secs(4)),
            "retrying in 4s · attempt 3"
        );
    }
}
