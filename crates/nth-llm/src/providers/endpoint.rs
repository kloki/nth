//! One configured endpoint: where it is, how it is reached, and a client
//! per protocol a model there may speak. Which protocol a model speaks is
//! the catalogue's to say; the endpoint only holds the clients.

use nth_protocol::{ModelInfo, Origin};

use super::listing;
use crate::{
    catalog::{self, Catalog, Wire},
    chat_completions, messages, responses,
};

/// A configured endpoint whose key is set.
pub struct EndpointConfig {
    /// The config's key for it, and the prefix of its models' ids.
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    /// The models to offer instead of the endpoint's own list; empty asks.
    pub models: Vec<String>,
}

/// A configured endpoint whose key is not set. Remembered so a model of its
/// is refused with the reason, rather than sent to the default provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    pub id: String,
    pub api_key_env: String,
}

/// An endpoint ready to list and to stream.
pub(crate) struct Endpoint {
    pub(crate) origin: Origin,
    /// What the catalogue knows the endpoint by.
    base_url: String,
    api_key: String,
    /// The models to offer instead of asking the endpoint's `/models`, for
    /// an endpoint without one; empty asks.
    only: Vec<String>,
    http: reqwest::Client,
    pub(crate) chat: chat_completions::Client,
    pub(crate) messages: messages::Client,
    pub(crate) responses: responses::Client,
}

impl Endpoint {
    /// On the `http` client every endpoint shares, so they need one pool.
    pub(crate) fn new(config: EndpointConfig, http: reqwest::Client) -> Self {
        let base_url = config.base_url.trim_end_matches('/').to_string();
        let api_key = config.api_key;
        Self {
            origin: Origin {
                id: config.id,
                name: config.name,
            },
            chat: chat_completions::Client::new(http.clone(), base_url.clone(), api_key.clone()),
            messages: messages::Client::new(http.clone(), base_url.clone(), api_key.clone()),
            responses: responses::Client::new(http.clone(), base_url.clone(), api_key.clone()),
            base_url,
            api_key,
            only: config.models,
            http,
        }
    }

    /// The ids of the models to offer: the configured ones, else what the
    /// endpoint lists.
    pub(crate) async fn ids(&self) -> Result<Vec<String>, listing::Error> {
        if !self.only.is_empty() {
            return Ok(self.only.clone());
        }
        listing::listed(&self.http, &self.base_url, &self.api_key).await
    }

    /// What the catalogue knows about `ids`, which this endpoint serves.
    pub(crate) fn describe(&self, ids: Vec<String>, catalog: Option<&Catalog>) -> Vec<ModelInfo> {
        listing::select(ids, self.provider_in(catalog))
    }

    /// The protocol `model` speaks here, and what the catalogue knows
    /// about it. Chat completions when it says nothing.
    pub(crate) fn wire<'c>(
        &self,
        catalog: Option<&'c Catalog>,
        model: &str,
    ) -> (Wire, Option<&'c catalog::Model>) {
        let provider = self.provider_in(catalog);
        let wire = provider.and_then(|p| p.wire(model));
        (
            wire.unwrap_or(Wire::ChatCompletions),
            provider.and_then(|p| p.models.get(model)),
        )
    }

    fn provider_in<'c>(&self, catalog: Option<&'c Catalog>) -> Option<&'c catalog::Provider> {
        catalog.and_then(|c| catalog::provider_for(c, &self.base_url))
    }
}
