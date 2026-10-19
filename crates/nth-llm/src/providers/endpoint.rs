//! One configured endpoint: where it is, how it is reached, and a client
//! per protocol a model there may speak. Which protocol a model speaks is
//! the catalogue's to say; the endpoint only holds the clients. The
//! ChatGPT endpoint is the exception: it speaks responses, lists its
//! models from the Codex backend, and runs only while you are connected
//! (`chatgpt.rs`).

use std::{collections::HashMap, sync::Mutex};

use futures::stream::BoxStream;
use nth_protocol::{BoxError, Effort, ModelInfo, Origin, Request, StreamEvent};

use super::{Error, chatgpt, images, listing};
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

/// What decides how an endpoint signs, lists and picks a model's wire.
enum Kind {
    /// A `[provider.<id>]` block: signed with its key, listing `only` or
    /// else its `/models`, and the catalogue saying each model's wire.
    Configured {
        key: String,
        /// The models to offer instead of asking the endpoint's `/models`,
        /// for an endpoint without one; empty asks.
        only: Vec<String>,
    },
    /// The built-in one (`chatgpt.rs`): a ChatGPT login, always responses
    /// with OpenAI's catalogue entry, and the Codex backend's own list.
    ChatGpt {
        login: nth_auth::ChatGpt,
        /// The efforts each model took in the last list, by wire id. It is
        /// what you picked from, so it beats the catalogue's. A std mutex:
        /// nothing awaits while holding it.
        listed: Mutex<HashMap<String, Vec<Effort>>>,
    },
}

/// An endpoint ready to list and to stream.
pub(crate) struct Endpoint {
    pub(crate) origin: Origin,
    /// What the catalogue knows the endpoint by.
    base_url: String,
    kind: Kind,
    http: reqwest::Client,
    chat: chat_completions::Client,
    messages: messages::Client,
    responses: responses::Client,
}

impl Endpoint {
    /// On the `http` client every endpoint shares, so they need one pool.
    pub(crate) fn new(config: EndpointConfig, http: reqwest::Client) -> Self {
        let base_url = config.base_url.trim_end_matches('/').to_string();
        let kind = match config.credentials {
            Credentials::Key(key) => Kind::Configured {
                key,
                only: config.models,
            },
            Credentials::ChatGpt(login) => Kind::ChatGpt {
                login,
                listed: Mutex::default(),
            },
        };
        Self {
            origin: Origin {
                id: config.id,
                name: config.name,
            },
            chat: chat_completions::Client::new(http.clone(), base_url.clone()),
            messages: messages::Client::new(http.clone(), base_url.clone()),
            responses: responses::Client::new(http.clone(), base_url.clone()),
            base_url,
            kind,
            http,
        }
    }

    /// Whether its models can run now: ChatGPT's only while connected,
    /// asked each time, so connecting mid-run needs nothing rebuilt.
    pub(crate) fn available(&self) -> Result<(), Error> {
        match &self.kind {
            Kind::Configured { .. } => Ok(()),
            Kind::ChatGpt { login, .. } if login.connected() => Ok(()),
            Kind::ChatGpt { .. } => Err(Error::Auth(nth_auth::Error::NotConnected)),
        }
    }

    /// What the next request is signed with, refreshing a ChatGPT token
    /// that is running out.
    async fn auth(&self) -> Result<Auth, nth_auth::Error> {
        Ok(match &self.kind {
            Kind::Configured { key, .. } => Auth::Key(key.clone()),
            Kind::ChatGpt { login, .. } => Auth::ChatGpt(login.credentials().await?),
        })
    }

    /// The models to offer, with what the catalogue knows about them.
    pub(crate) async fn list(&self, catalog: &Snapshot) -> Result<Vec<ModelInfo>, listing::Error> {
        match &self.kind {
            Kind::ChatGpt { listed, .. } => {
                let (catalog, auth) = futures::join!(catalog.get(), self.auth());
                let models = chatgpt::list(&self.http, &self.base_url, &auth?, catalog).await?;
                *listed.lock().expect("listed lock poisoned") = models
                    .iter()
                    .map(|m| (m.id.clone(), m.efforts.clone()))
                    .collect();
                Ok(models)
            }
            Kind::Configured { only, .. } => {
                // At once: the catalogue's deadline must not delay the list.
                let (catalog, ids) = futures::join!(catalog.get(), self.ids(only));
                Ok(listing::select(
                    ids?,
                    catalog,
                    catalog.and_then(|c| self.provider_in(c)).map(|(_, p)| p),
                ))
            }
        }
    }

    /// The efforts `model` took in the endpoint's last list, if it said.
    fn listed_efforts(&self, model: &str) -> Option<Vec<Effort>> {
        match &self.kind {
            Kind::ChatGpt { listed, .. } => listed
                .lock()
                .expect("listed lock poisoned")
                .get(model)
                .cloned(),
            Kind::Configured { .. } => None,
        }
    }

    #[cfg(test)]
    pub(super) fn listed(&self) -> &Mutex<HashMap<String, Vec<Effort>>> {
        match &self.kind {
            Kind::ChatGpt { listed, .. } => listed,
            Kind::Configured { .. } => panic!("only ChatGPT remembers its list"),
        }
    }

    /// The ids of the models to offer: the configured ones, else what the
    /// endpoint lists.
    async fn ids(&self, only: &[String]) -> Result<Vec<String>, listing::Error> {
        if !only.is_empty() {
            return Ok(only.to_vec());
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
        if let Kind::ChatGpt { .. } = self.kind {
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
    fn takes_cache_key(&self, catalog: Option<&Catalog>) -> bool {
        catalog
            .and_then(|c| self.provider_in(c))
            .is_some_and(|(id, _)| chat_completions::takes_cache_key(id))
    }

    fn provider_in<'c>(&self, catalog: &'c Catalog) -> Option<(&'c str, &'c catalog::Provider)> {
        catalog::provider_for(catalog, &self.origin.id, &self.base_url)
    }

    /// Streams `request`, whose model is the id the endpoint knows, over
    /// the protocol the model speaks here, with what that protocol needs.
    pub(crate) async fn stream(
        &self,
        request: Request<'_>,
        catalog: &Snapshot,
    ) -> Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError> {
        // A ChatGPT login that cannot refresh fails here, for good.
        let auth = self.auth().await?;
        let catalog = catalog.get().await;
        let (wire, known) = self.wire(catalog, request.model);
        // Whatever picked the effort (the config, a resumed session, a
        // model switch), a known model is sent only a level it takes: one
        // the endpoint listed, which the picker showed, else one the
        // catalogue says.
        let listed = self.listed_efforts(request.model);
        let efforts = listed.clone().or_else(|| known.map(|k| k.efforts()));
        let request = match efforts {
            Some(efforts) => Request {
                effort: request.effort.nearest(&efforts),
                ..request
            },
            None => request,
        };
        let prepared = images::prepare(request.messages, known.map(|k| k.takes_images()));
        let request = match &prepared {
            Some(messages) => Request {
                messages,
                ..request
            },
            None => request,
        };
        match wire {
            Wire::ChatCompletions => {
                let cache_key = self.takes_cache_key(catalog).then_some(request.session_id);
                self.chat.stream(request, cache_key, &auth).await
            }
            Wire::Messages => self.messages.stream(request, known, &auth).await,
            Wire::Responses => {
                // A model listed with efforts reasons, whatever the
                // catalogue says, or whether it knows the model at all.
                let reasons = listed.is_some_and(|e| !e.is_empty())
                    || known.and_then(|m| m.reasoning).unwrap_or(false);
                self.responses.stream(request, reasons, &auth).await
            }
        }
    }
}
