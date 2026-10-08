use std::time::Duration;

use futures::{future::BoxFuture, stream::BoxStream};
use serde::{Deserialize, Serialize};

use crate::{Message, ToolCall, ToolSpec};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// What a provider says about an error it produced: whether another attempt
/// could work, and the server's `Retry-After` when it sent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    /// The server asked to wait this long first, instead of the backoff.
    pub after: Option<Duration>,
}

#[derive(Debug, Clone, Copy)]
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

/// A model and the effort it runs at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Llm {
    pub model: String,
    pub effort: Effort,
}

/// How hard a reasoning model thinks before it answers. The levels are
/// models.dev's; a model takes only some of them (`ModelInfo::efforts`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Leaves the choice to the model, for endpoints that reject the field.
    #[default]
    Default,
    /// Asks the model not to think, where it can be told so.
    None,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    /// Every level, least thinking first after `Default`.
    pub const ALL: [Effort; 8] = [
        Effort::Default,
        Effort::None,
        Effort::Minimal,
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::XHigh,
        Effort::Max,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Effort::Default => "default",
            Effort::None => "none",
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        }
    }

    /// The level named `name` on the wire, if nth knows it.
    pub fn from_name(name: &str) -> Option<Effort> {
        Self::ALL.into_iter().find(|e| e.name() == name)
    }

    /// The value to send, if any.
    pub fn wire(self) -> Option<&'static str> {
        (self != Effort::Default).then(|| self.name())
    }

    /// The level of `allowed` closest to this one, rounding up on a tie, so
    /// a model is never sent a level it refuses. `Default` always stays, and
    /// is all a model without levels can be sent.
    pub fn nearest(self, allowed: &[Effort]) -> Effort {
        if self == Effort::Default || allowed.contains(&self) {
            return self;
        }
        let at = self.index();
        allowed
            .iter()
            .copied()
            .filter(|&e| e != Effort::Default)
            .min_by_key(|e| {
                let i = e.index();
                // Below comes after above at the same distance.
                (i.abs_diff(at), i < at)
            })
            .unwrap_or(Effort::Default)
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

/// The provider a model is served by, when the `Provider` fronts several.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Origin {
    /// The config's key for it, and the prefix of its models' ids.
    pub id: String,
    /// What a listing shows.
    pub name: String,
}

/// A model a provider can serve, with limits when they are known.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelInfo {
    /// What a session names: `provider/model` when the model has an origin,
    /// else the id the endpoint knows it by.
    pub id: String,
    pub name: Option<String>,
    /// Context window in tokens.
    pub context: Option<u64>,
    /// Maximum output tokens.
    pub output: Option<u64>,
    /// The efforts it takes, least first; none when there is nothing to
    /// pick.
    pub efforts: Vec<Effort>,
    pub origin: Option<Origin>,
}

/// What listing the models gave: the models, and the providers that could
/// not be asked, so a wrong key shows up rather than a missing group.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Listing {
    pub models: Vec<ModelInfo>,
    pub failed: Vec<Failed>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Failed {
    pub origin: Origin,
    pub error: String,
}

impl From<Vec<ModelInfo>> for Listing {
    fn from(models: Vec<ModelInfo>) -> Self {
        Self {
            models,
            failed: Vec::new(),
        }
    }
}

impl ModelInfo {
    /// The id the endpoint knows the model by: `id` without the origin's
    /// prefix. The one place that knows the two are joined by a `/`.
    pub fn wire_id(&self) -> &str {
        match &self.origin {
            Some(origin) => self
                .id
                .strip_prefix(origin.id.as_str())
                .and_then(|rest| rest.strip_prefix('/'))
                .unwrap_or(&self.id),
            None => &self.id,
        }
    }

    /// The known limits, such as `1M ctx · 128k out`; empty when none are.
    pub fn limits(&self) -> String {
        // Rounded to the nearest, with a decimal only where a window like
        // 1.5M would otherwise read as 1M.
        let tokens = |n: u64| match n {
            1_000_000.. => match (n + 50_000) / 100_000 {
                tenths if tenths % 10 == 0 => format!("{}M", tenths / 10),
                tenths => format!("{}.{}M", tenths / 10, tenths % 10),
            },
            1_000.. => format!("{}k", (n + 500) / 1_000),
            _ => n.to_string(),
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
    /// Only models this provider can actually talk to. An error means
    /// nothing could be listed; a provider fronting several reports the
    /// ones that failed in the listing.
    fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>>;

    fn stream<'a>(
        &'a self,
        request: Request<'a>,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>;

    /// Whether `error`, one this provider produced, may be retried, with the
    /// server's `Retry-After` when it gave one. The default never retries.
    fn retry(&self, error: &BoxError) -> Option<Retry> {
        let _ = error;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_effort_snaps_to_the_nearest_allowed_level() {
        use Effort::*;
        assert_eq!(Low.nearest(&[High, Max]), High);
        assert_eq!(Max.nearest(&[Low, Medium, High]), High);
        assert_eq!(Medium.nearest(&[Low, High]), High, "a tie rounds up");
        assert_eq!(High.nearest(&[Low, High]), High);
        assert_eq!(Default.nearest(&[Max]), Default);
        assert_eq!(High.nearest(&[]), Default, "nothing to send");
        assert_eq!(Default.wire(), Option::None);
        assert_eq!(XHigh.wire(), Some("xhigh"));
        assert_eq!(Effort::from_name("minimal"), Some(Minimal));
        assert_eq!(Effort::from_name("ultra"), Option::None);
    }

    #[test]
    fn limits_show_what_is_known() {
        let model = ModelInfo {
            id: "m".into(),
            name: None,
            context: Some(1_000_000),
            output: Some(131_072),
            efforts: vec![Effort::High],
            origin: None,
        };
        assert_eq!(model.limits(), "1M ctx · 131k out");
        let unknown = ModelInfo {
            context: None,
            output: None,
            ..model.clone()
        };
        assert_eq!(unknown.limits(), "");
        let odd = ModelInfo {
            context: Some(1_500_000),
            output: Some(800),
            ..model.clone()
        };
        assert_eq!(odd.limits(), "1.5M ctx · 800 out");
        let rounded = ModelInfo {
            context: Some(1_048_576),
            output: Some(65_536),
            ..model
        };
        assert_eq!(rounded.limits(), "1M ctx · 66k out");
    }

    #[test]
    fn the_wire_id_drops_the_origin_prefix_only() {
        let bare = ModelInfo {
            id: "z-ai/glm-5.2".into(),
            name: None,
            context: None,
            output: None,
            efforts: Vec::new(),
            origin: None,
        };
        assert_eq!(bare.wire_id(), "z-ai/glm-5.2");
        let served = ModelInfo {
            id: "lyceum/z-ai/glm-5.2".into(),
            origin: Some(Origin {
                id: "lyceum".into(),
                name: "Lyceum".into(),
            }),
            ..bare.clone()
        };
        assert_eq!(served.wire_id(), "z-ai/glm-5.2");
        let mismatched = ModelInfo {
            id: "lyceumx".into(),
            ..served
        };
        assert_eq!(mismatched.wire_id(), "lyceumx", "no slash, so no prefix");
    }
}
