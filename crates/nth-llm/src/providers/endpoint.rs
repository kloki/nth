//! One configured endpoint: where it is, how it is reached, and a client
//! per protocol a model there may speak. Which protocol a model speaks is
//! the catalogue's to say; the endpoint only holds the clients. The
//! ChatGPT endpoint is the exception: it speaks responses, lists no
//! `/models`, and runs only while you are connected (`chatgpt.rs`).

use nth_protocol::{ModelInfo, Origin};

use super::{Error, chatgpt, listing};
use crate::{
    auth::{Auth, Credentials},
    catalog::{self, Catalog, Snapshot, Wire},
    chat_completions, messages, responses,
};

/// An endpoint that can sign its requests: a configured one whose key is
/// set, or ChatGPT (`EndpointConfig::chatgpt`).
pub struct EndpointConfig {
    /// The config's key for it, and the prefix of its models' ids.
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub credentials: Credentials,
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
    credentials: Credentials,
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
        Self {
            origin: Origin {
                id: config.id,
                name: config.name,
            },
            chat: chat_completions::Client::new(http.clone(), base_url.clone()),
            messages: messages::Client::new(http.clone(), base_url.clone()),
            responses: responses::Client::new(http.clone(), base_url.clone()),
            base_url,
            credentials: config.credentials,
            only: config.models,
            http,
        }
    }

    /// Whether its models can run now: ChatGPT's only while connected,
    /// asked each time.
    pub(crate) fn available(&self) -> Result<(), Error> {
        match self.credentials.connected() {
            true => Ok(()),
            false => Err(Error::Auth(nth_auth::Error::NotConnected)),
        }
    }

    /// What the next request is signed with.
    pub(crate) async fn auth(&self) -> Result<Auth, nth_auth::Error> {
        self.credentials.resolve().await
    }

    fn chatgpt(&self) -> bool {
        matches!(self.credentials, Credentials::ChatGpt(_))
    }

    /// The models to offer, with what the catalogue knows about them.
    pub(crate) async fn list(&self, catalog: &Snapshot) -> Result<Vec<ModelInfo>, listing::Error> {
        if self.chatgpt() {
            return Ok(chatgpt::models(catalog.get().await));
        }
        // At once: the catalogue's deadline must not delay the list.
        let (catalog, ids) = futures::join!(catalog.get(), self.ids());
        Ok(listing::select(
            ids?,
            catalog,
            catalog.and_then(|c| self.provider_in(c)).map(|(_, p)| p),
        ))
    }

    /// The ids of the models to offer: the configured ones, else what the
    /// endpoint lists.
    async fn ids(&self) -> Result<Vec<String>, listing::Error> {
        if !self.only.is_empty() {
            return Ok(self.only.clone());
        }
        listing::listed(&self.http, &self.base_url, &self.auth().await?).await
    }

    /// The protocol `model` speaks here, and what the catalogue knows
    /// about it. Chat completions when it says nothing, and for an
    /// endpoint it has no entry for, whose model it may still know from
    /// another provider. Always responses on ChatGPT, with OpenAI's entry.
    pub(crate) fn wire<'c>(
        &self,
        catalog: Option<&'c Catalog>,
        model: &str,
    ) -> (Wire, Option<&'c catalog::Model>) {
        if self.chatgpt() {
            return (Wire::Responses, chatgpt::known(catalog, model));
        }
        let Some(catalog) = catalog else {
            return (Wire::ChatCompletions, None);
        };
        match self.provider_in(catalog) {
            Some((_, provider)) => (
                provider.wire(model).unwrap_or(Wire::ChatCompletions),
                provider.models.get(model),
            ),
            None => (Wire::ChatCompletions, catalog::borrowed(catalog, model)),
        }
    }

    /// Whether its chat completions take a `prompt_cache_key`: only an
    /// endpoint the catalogue knows, of a provider known to accept it.
    pub(crate) fn takes_cache_key(&self, catalog: Option<&Catalog>) -> bool {
        catalog
            .and_then(|c| self.provider_in(c))
            .is_some_and(|(id, _)| chat_completions::takes_cache_key(id))
    }

    fn provider_in<'c>(&self, catalog: &'c Catalog) -> Option<(&'c str, &'c catalog::Provider)> {
        catalog::provider_for(catalog, &self.origin.id, &self.base_url)
    }
}
