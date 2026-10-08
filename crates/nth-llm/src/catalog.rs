//! The models.dev catalogue: what is known about every provider's models,
//! their protocol and their limits. Fetched once and shared: the listing
//! reads names and limits from it, and a request the protocol to speak.

use std::{collections::HashMap, sync::Arc, time::Duration};

use serde::Deserialize;
use tokio::sync::OnceCell;

use crate::chat_completions::{Error, USER_AGENT, success};

const URL: &str = "https://models.dev/api.json";
/// The catalogue only refines a listing, so a stall must not hold it up.
const TIMEOUT: Duration = Duration::from_secs(5);

/// Providers by their models.dev id.
pub(crate) type Catalog = HashMap<String, Provider>;

#[derive(Deserialize)]
pub(crate) struct Provider {
    pub(crate) npm: Option<String>,
    pub(crate) api: Option<String>,
    #[serde(default)]
    pub(crate) models: HashMap<String, Model>,
}

#[derive(Deserialize)]
pub(crate) struct Model {
    pub(crate) name: Option<String>,
    pub(crate) reasoning: Option<bool>,
    pub(crate) limit: Option<Limit>,
    /// Set when this model needs a different SDK, and so a different wire
    /// protocol, than its provider's default.
    pub(crate) provider: Option<Override>,
}

#[derive(Deserialize)]
pub(crate) struct Limit {
    pub(crate) context: Option<u64>,
    pub(crate) output: Option<u64>,
}

#[derive(Deserialize)]
pub(crate) struct Override {
    pub(crate) npm: Option<String>,
}

/// The wire protocols nth speaks, by the SDK the catalogue names for a model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Wire {
    ChatCompletions,
    Messages,
}

impl Wire {
    fn from_npm(npm: &str) -> Option<Self> {
        match npm {
            "@ai-sdk/openai-compatible" => Some(Self::ChatCompletions),
            "@ai-sdk/anthropic" => Some(Self::Messages),
            _ => None,
        }
    }
}

impl Provider {
    /// The protocol `model` speaks here; none when nth does not speak it.
    /// A model the catalogue does not know, or one without an SDK, is
    /// taken to speak chat completions, since that is what an endpoint
    /// without an entry is assumed to be for.
    pub(crate) fn wire(&self, model: &str) -> Option<Wire> {
        let Some(entry) = self.models.get(model) else {
            return Some(Wire::ChatCompletions);
        };
        let npm = entry.provider.as_ref().and_then(|o| o.npm.as_deref());
        match npm.or(self.npm.as_deref()) {
            Some(npm) => Wire::from_npm(npm),
            None => Some(Wire::ChatCompletions),
        }
    }
}

/// The catalogue, fetched by whichever needs it first and then kept for
/// the life of the process. A failed fetch is kept too, as no catalogue:
/// every model then speaks chat completions, as with an unknown endpoint.
#[derive(Clone)]
pub(crate) struct Snapshot {
    http: reqwest::Client,
    catalog: Arc<OnceCell<Option<Catalog>>>,
}

impl Snapshot {
    pub(crate) fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            catalog: Arc::new(OnceCell::new()),
        }
    }

    /// The catalogue, waiting for the fetch if it is still under way.
    pub(crate) async fn get(&self) -> Option<&Catalog> {
        self.catalog
            .get_or_init(|| async { fetch(&self.http).await.ok() })
            .await
            .as_ref()
    }

    #[cfg(test)]
    pub(crate) fn of(catalog: Catalog) -> Self {
        Self {
            http: reqwest::Client::new(),
            catalog: Arc::new(OnceCell::new_with(Some(Some(catalog)))),
        }
    }
}

pub(crate) async fn fetch(http: &reqwest::Client) -> Result<Catalog, Error> {
    let response = http
        .get(URL)
        .timeout(TIMEOUT)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await?;
    Ok(success(response).await?.json().await?)
}

/// The catalogue's entry for the provider serving `base_url`, if it has one.
pub(crate) fn provider_for<'a>(catalog: &'a Catalog, base_url: &str) -> Option<&'a Provider> {
    let base_url = base_url.trim_end_matches('/');
    catalog
        .values()
        .find(|p| p.api.as_deref().map(|a| a.trim_end_matches('/')) == Some(base_url))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../tests/fixtures/models_dev.json");

    #[test]
    fn the_sdk_picks_the_wire() {
        let catalog: Catalog = serde_json::from_str(FIXTURE).expect("valid fixture");
        let go = provider_for(&catalog, "https://opencode.ai/zen/go/v1").expect("go entry");
        assert_eq!(go.wire("glm-5.3"), Some(Wire::ChatCompletions));
        assert_eq!(go.wire("minimax-m3"), Some(Wire::Messages));
        assert_eq!(go.wire("grok-4.7"), None, "responses is not spoken");
        assert_eq!(go.wire("unlisted"), Some(Wire::ChatCompletions));

        let other = &catalog["other"];
        assert_eq!(
            other.wire("claude-x"),
            Some(Wire::Messages),
            "the provider's SDK"
        );
        assert_eq!(other.wire("unlisted"), Some(Wire::ChatCompletions));
    }
}
