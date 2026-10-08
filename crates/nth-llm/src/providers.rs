//! Several endpoints behind one `Provider`. A model id is `provider/model`:
//! the prefix picks the endpoint, the rest goes on the wire. An id without
//! a known prefix goes whole to the default provider, so sessions saved
//! before there were several keep working. The catalogue then says which
//! protocol the model speaks there.

use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
use nth_protocol::{
    BoxError, Failed, Listing, ModelInfo, Origin, Provider, Request, Retry, StreamEvent,
};
use tokio::task::JoinHandle;

use crate::{
    catalog::{self, Snapshot, Wire},
    chat_completions::{self, ChatClient},
    messages, responses,
};

/// A configured endpoint whose key is set.
pub struct Endpoint {
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

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no provider has its API key set")]
    NoProviders,
    #[error("{id} is configured but {api_key_env} is not set")]
    Unavailable { id: String, api_key_env: String },
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
}

/// One endpoint, with a client per protocol it may speak.
struct Client {
    origin: Origin,
    /// What the catalogue knows the endpoint by.
    base_url: String,
    chat: ChatClient,
    messages: messages::Client,
    responses: responses::Client,
}

pub struct Providers {
    /// Index of the provider a bare id goes to: the one the config's model
    /// names, else the first.
    default: usize,
    clients: Vec<Client>,
    unavailable: Vec<Unavailable>,
    catalog: Snapshot,
    /// The catalogue fetch started with the providers, so the first
    /// request need not wait for it.
    fetch: Option<JoinHandle<()>>,
}

impl Providers {
    /// `default_model` picks the default provider; a model of an
    /// unavailable provider is an error here rather than at the first turn.
    pub fn new(
        endpoints: Vec<Endpoint>,
        unavailable: Vec<Unavailable>,
        default_model: &str,
    ) -> Result<Self, Error> {
        let http = crate::http::client()?;
        let catalog = Snapshot::new(http.clone());
        let mut providers = Self::build(endpoints, unavailable, default_model, http, catalog)?;
        // Without a runtime, as in a test, the first request fetches it.
        providers.fetch = tokio::runtime::Handle::try_current().ok().map(|runtime| {
            let catalog = providers.catalog.clone();
            runtime.spawn(async move {
                catalog.get().await;
            })
        });
        Ok(providers)
    }

    fn build(
        endpoints: Vec<Endpoint>,
        unavailable: Vec<Unavailable>,
        default_model: &str,
        http: reqwest::Client,
        catalog: Snapshot,
    ) -> Result<Self, Error> {
        if endpoints.is_empty() {
            return Err(Error::NoProviders);
        }
        let clients = endpoints
            .into_iter()
            .map(|endpoint| Client {
                origin: Origin {
                    id: endpoint.id,
                    name: endpoint.name,
                },
                base_url: endpoint.base_url.clone(),
                chat: ChatClient::with_http(
                    http.clone(),
                    endpoint.base_url.clone(),
                    endpoint.api_key.clone(),
                )
                .only(endpoint.models),
                messages: messages::Client::new(
                    http.clone(),
                    endpoint.base_url.clone(),
                    endpoint.api_key.clone(),
                ),
                responses: responses::Client::new(
                    http.clone(),
                    endpoint.base_url,
                    endpoint.api_key,
                ),
            })
            .collect();
        let mut providers = Self {
            default: 0,
            clients,
            unavailable,
            catalog,
            fetch: None,
        };
        providers.default = providers.route(default_model)?.0;
        Ok(providers)
    }

    /// `model` as `provider/model`, so a bare id shows and compares like the
    /// listed ones. An id this cannot route is returned as it is.
    pub fn qualify(&self, model: &str) -> String {
        match self.route(model) {
            Ok((i, wire)) => format!("{}/{wire}", self.clients[i].origin.id),
            Err(_) => model.to_string(),
        }
    }

    /// Whether `model` can run: an id of an unavailable provider cannot.
    pub fn check(&self, model: &str) -> Result<(), Error> {
        self.route(model).map(|_| ())
    }

    /// The client `model` is for, and the id it knows the model by.
    fn route<'a>(&self, model: &'a str) -> Result<(usize, &'a str), Error> {
        if let Some((prefix, wire)) = model.split_once('/') {
            if let Some(i) = self.clients.iter().position(|c| c.origin.id == prefix) {
                return Ok((i, wire));
            }
            if let Some(unavailable) = self.unavailable.iter().find(|u| u.id == prefix) {
                return Err(Error::Unavailable {
                    id: unavailable.id.clone(),
                    api_key_env: unavailable.api_key_env.clone(),
                });
            }
        }
        Ok((self.default, model))
    }

    /// The protocol `model`, as client `i` knows it, speaks there, and what
    /// the catalogue knows about it. Chat completions when it says nothing.
    async fn wire(&self, i: usize, model: &str) -> (Wire, Option<&catalog::Model>) {
        let provider = self
            .catalog
            .get()
            .await
            .and_then(|c| catalog::provider_for(c, &self.clients[i].base_url));
        let wire = provider.and_then(|p| p.wire(model));
        (
            wire.unwrap_or(Wire::ChatCompletions),
            provider.and_then(|p| p.models.get(model)),
        )
    }
}

impl Drop for Providers {
    fn drop(&mut self) {
        if let Some(fetch) = &self.fetch {
            fetch.abort();
        }
    }
}

impl Provider for Providers {
    fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
        async move {
            // Everything at once: the catalogue's deadline must not delay the
            // lists, and one slow endpoint must not delay the others' start.
            let ids = self.clients.iter().map(|c| c.chat.ids());
            let (catalog, ids) = futures::join!(self.catalog.get(), futures::future::join_all(ids));
            let listed = self
                .clients
                .iter()
                .zip(ids)
                .map(|(client, ids)| {
                    let models = ids.map(|ids| client.chat.describe(ids, catalog));
                    (client.origin.clone(), models)
                })
                .collect();
            merge(listed).map_err(BoxError::from)
        }
        .boxed()
    }

    fn stream<'a>(
        &'a self,
        request: Request<'a>,
    ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>> {
        async move {
            let (i, model) = self.route(request.model)?;
            let request = Request { model, ..request };
            let client = &self.clients[i];
            match self.wire(i, model).await {
                (Wire::ChatCompletions, _) => client.chat.stream(request).await,
                (Wire::Messages, known) => {
                    let output = known.and_then(|m| m.limit.as_ref()?.output);
                    Ok(client
                        .messages
                        .stream(request, messages::max_tokens(output))
                        .await?)
                }
                (Wire::Responses, known) => {
                    let reasons = known.and_then(|m| m.reasoning).unwrap_or(false);
                    Ok(client.responses.stream(request, reasons).await?)
                }
            }
        }
        .boxed()
    }

    fn retry(&self, error: &BoxError) -> Option<Retry> {
        // Each protocol recognises only its own errors.
        chat_completions::retry(error)
            .or_else(|| messages::retry(error))
            .or_else(|| responses::retry(error))
    }
}

/// One listing from every provider's: ids prefixed with their provider,
/// and a provider that could not list reported beside them. Only when
/// none could is the listing itself a failure.
fn merge<E: std::fmt::Display>(
    listed: Vec<(Origin, Result<Vec<ModelInfo>, E>)>,
) -> Result<Listing, E> {
    let mut listing = Listing::default();
    let mut errors = Vec::new();
    let mut any_listed = false;
    for (origin, models) in listed {
        match models {
            Ok(models) => {
                any_listed = true;
                listing
                    .models
                    .extend(models.into_iter().map(|model| ModelInfo {
                        id: format!("{}/{}", origin.id, model.id),
                        origin: Some(origin.clone()),
                        ..model
                    }));
            }
            Err(error) => {
                listing.failed.push(Failed {
                    origin,
                    error: error.to_string(),
                });
                errors.push(error);
            }
        }
    }
    match errors.into_iter().next() {
        Some(error) if !any_listed => Err(error),
        _ => Ok(listing),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(id: &str) -> Endpoint {
        Endpoint {
            id: id.into(),
            name: id.to_uppercase(),
            base_url: format!("https://{id}.test/v1"),
            api_key: "key".into(),
            models: Vec::new(),
        }
    }

    fn origin(id: &str) -> Origin {
        Origin {
            id: id.into(),
            name: id.to_uppercase(),
        }
    }

    fn model(id: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: None,
            context: None,
            output: None,
            reasoning: true,
            origin: None,
        }
    }

    fn providers(default_model: &str) -> Providers {
        Providers::new(
            vec![endpoint("lyceum"), endpoint("opencode")],
            vec![Unavailable {
                id: "muted".into(),
                api_key_env: "MUTED_API_KEY".into(),
            }],
            default_model,
        )
        .expect("two providers")
    }

    #[test]
    fn the_prefix_picks_the_provider_and_the_rest_goes_on_the_wire() {
        let providers = providers("opencode/deepseek");
        assert_eq!(
            providers.route("lyceum/z-ai/glm-5.2").ok(),
            Some((0, "z-ai/glm-5.2"))
        );
        assert_eq!(providers.route("opencode/kimi").ok(), Some((1, "kimi")));
    }

    #[test]
    fn an_unknown_prefix_or_none_goes_whole_to_the_default() {
        let providers = providers("opencode/deepseek");
        assert_eq!(providers.route("deepseek").ok(), Some((1, "deepseek")));
        assert_eq!(
            providers.route("z-ai/glm-5.2").ok(),
            Some((1, "z-ai/glm-5.2"))
        );
        assert_eq!(providers.qualify("deepseek"), "opencode/deepseek");
        assert_eq!(providers.qualify("opencode/deepseek"), "opencode/deepseek");
        assert_eq!(providers.qualify("lyceum/glm"), "lyceum/glm");
    }

    #[test]
    fn a_bare_default_model_makes_the_first_provider_the_default() {
        let providers = providers("deepseek");
        assert_eq!(providers.route("deepseek").ok(), Some((0, "deepseek")));
    }

    #[test]
    fn a_provider_without_its_key_refuses_its_models() {
        let providers = providers("opencode/deepseek");
        let error = providers.route("muted/glm").expect_err("no key");
        assert_eq!(
            error.to_string(),
            "muted is configured but MUTED_API_KEY is not set"
        );
        assert_eq!(providers.qualify("muted/glm"), "muted/glm");

        let error = Providers::new(vec![endpoint("lyceum")], Vec::new(), "x").err();
        assert!(error.is_none(), "a bare default is fine");
        let error = Providers::new(
            vec![endpoint("lyceum")],
            vec![Unavailable {
                id: "muted".into(),
                api_key_env: "MUTED_API_KEY".into(),
            }],
            "muted/glm",
        )
        .err();
        assert!(matches!(error, Some(Error::Unavailable { .. })));
        assert!(matches!(
            Providers::new(Vec::new(), Vec::new(), "x").err(),
            Some(Error::NoProviders)
        ));
    }

    #[tokio::test]
    async fn the_catalogue_picks_the_protocol() {
        let catalog = serde_json::from_str(include_str!("../tests/fixtures/models_dev.json"))
            .expect("valid fixture");
        let go = Endpoint {
            base_url: "https://opencode.ai/zen/go/v1/".into(),
            ..endpoint("opencode")
        };
        let providers = Providers::build(
            vec![endpoint("lyceum"), go],
            Vec::new(),
            "opencode/glm-5.3",
            reqwest::Client::new(),
            Snapshot::of(catalog),
        )
        .expect("two providers");
        let wire = async |model| {
            let (i, model) = providers.route(model).expect("routes");
            let (wire, known) = providers.wire(i, model).await;
            (wire, known.and_then(|m| m.limit.as_ref()?.output))
        };

        assert_eq!(
            wire("opencode/minimax-m3").await,
            (Wire::Messages, Some(131_072))
        );
        assert_eq!(
            wire("opencode/grok-4.7").await,
            (Wire::Responses, Some(500_000))
        );
        assert_eq!(
            wire("glm-5.3").await,
            (Wire::ChatCompletions, Some(131_072))
        );
        assert_eq!(
            wire("opencode/unlisted").await,
            (Wire::ChatCompletions, None)
        );
        assert_eq!(
            wire("lyceum/minimax-m3").await,
            (Wire::ChatCompletions, None),
            "an endpoint the catalogue does not know"
        );
    }

    #[test]
    fn merging_prefixes_ids_and_reports_the_providers_that_failed() {
        let listing = merge(vec![
            (origin("lyceum"), Ok(vec![model("z-ai/glm-5.2")])),
            (origin("opencode"), Err("401: bad key")),
        ])
        .expect("one listed");

        assert_eq!(
            listing.models,
            [ModelInfo {
                id: "lyceum/z-ai/glm-5.2".into(),
                origin: Some(origin("lyceum")),
                ..model("")
            }]
        );
        assert_eq!(listing.models[0].wire_id(), "z-ai/glm-5.2");
        assert_eq!(
            listing.failed,
            [Failed {
                origin: origin("opencode"),
                error: "401: bad key".into()
            }]
        );
    }

    #[test]
    fn merging_fails_only_when_every_provider_did() {
        let error = merge::<&str>(vec![
            (origin("lyceum"), Err("offline")),
            (origin("opencode"), Err("401")),
        ])
        .expect_err("nothing listed");
        assert_eq!(error, "offline");
        assert_eq!(merge::<&str>(Vec::new()), Ok(Listing::default()));
    }
}
