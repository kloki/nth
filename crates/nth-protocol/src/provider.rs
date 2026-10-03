use futures::{future::BoxFuture, stream::BoxStream};
use serde::{Deserialize, Serialize};

use crate::{Message, ToolCall, ToolSpec};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

pub struct Request<'a> {
    /// Per request rather than per provider, so a session can switch models
    /// without rebuilding its client.
    pub model: &'a str,
    /// Per request too, so one client serves successive sessions. It must
    /// stay stable within a conversation: Go routes and caches prompts on it.
    pub session_id: &'a str,
    pub effort: Effort,
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
}

/// How hard a reasoning model thinks before it answers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Leaves the choice to the model, for endpoints that reject the field.
    #[default]
    Default,
    Low,
    Medium,
    High,
}

impl Effort {
    pub const ALL: [Effort; 4] = [Effort::Default, Effort::Low, Effort::Medium, Effort::High];

    pub fn name(self) -> &'static str {
        match self {
            Effort::Default => "default",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }

    /// The value to send, if any.
    pub fn wire(self) -> Option<&'static str> {
        (self != Effort::Default).then(|| self.name())
    }

    /// One step up, staying at the top.
    pub fn next(self) -> Effort {
        let i = self.index();
        Self::ALL[(i + 1).min(Self::ALL.len() - 1)]
    }

    /// One step down, staying at the bottom.
    pub fn prev(self) -> Effort {
        Self::ALL[self.index().saturating_sub(1)]
    }

    fn index(self) -> usize {
        Self::ALL.iter().position(|&e| e == self).unwrap_or(0)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    ReasoningDelta(String),
    /// Emitted once the call's arguments are complete.
    ToolCall(ToolCall),
    Usage(Usage),
}

/// Tokens one model reply used, as the provider reports them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Everything sent: system prompt, history and tool results.
    pub input: u64,
    pub output: u64,
}

impl Usage {
    /// How much of the context window the conversation now fills: the
    /// reply becomes history for the next request.
    pub fn context(self) -> u64 {
        self.input + self.output
    }
}

/// A model a provider can serve, with limits when they are known.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: Option<String>,
    /// Context window in tokens.
    pub context: Option<u64>,
    /// Maximum output tokens.
    pub output: Option<u64>,
    /// Whether it takes a reasoning effort.
    pub reasoning: bool,
}

impl ModelInfo {
    /// The known limits, such as `1M ctx · 128k out`; empty when none are.
    pub fn limits(&self) -> String {
        let tokens = |n: u64| match n {
            1_000_000.. => format!("{}M", n / 1_000_000),
            _ => format!("{}k", n / 1_000),
        };
        [
            self.context.map(|n| format!("{} ctx", tokens(n))),
            self.output.map(|n| format!("{} out", tokens(n))),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ")
    }
}

pub trait Provider: Send + Sync {
    /// Only models this provider can actually talk to.
    fn models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, BoxError>>;

    fn stream<'a>(
        &'a self,
        request: Request<'a>,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_steps_stop_at_the_ends() {
        assert_eq!(Effort::Default.prev(), Effort::Default);
        assert_eq!(Effort::Default.next(), Effort::Low);
        assert_eq!(Effort::Medium.next(), Effort::High);
        assert_eq!(Effort::High.next(), Effort::High);
        assert_eq!(Effort::Default.wire(), None);
        assert_eq!(Effort::High.wire(), Some("high"));
    }

    #[test]
    fn limits_show_what_is_known() {
        let model = ModelInfo {
            id: "m".into(),
            name: None,
            context: Some(1_000_000),
            output: Some(131_072),
            reasoning: true,
        };
        assert_eq!(model.limits(), "1M ctx · 131k out");
        let unknown = ModelInfo {
            context: None,
            output: None,
            ..model
        };
        assert_eq!(unknown.limits(), "");
    }
}
