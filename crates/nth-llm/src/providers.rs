//! Several endpoints behind one `Provider`. A model id is `provider/model`:
//! the prefix picks the endpoint, the rest goes on the wire. An id without
//! a known prefix goes whole to the default provider, so sessions saved
//! before there were several keep working.

use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
use nth_protocol::{
    BoxError, Failed, Listing, ModelInfo, Origin, Provider, Request, Retry, StreamEvent,
};

use crate::{
    catalog,
    chat_completions::{self, ChatClient},
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
    #[error(transparent)]
    Http(#[from] chat_completions::Error),
}

struct Client {
    origin: Origin,
    inner: ChatClient,
}

pub struct Providers {
    /// Index of the provider a bare id goes to: the one the config's model
    /// names, else the first.
    default: usize,
    clients: Vec<Client>,
    unavailable: Vec<Unavailable>,
    /// Shared by every client and the catalogue fetch.
    http: reqwest::Client,
}

impl Providers {
    /// `default_model` picks the default provider; a model of an
    /// unavailable provider is an error here rather than at the first turn.
    pub fn new(
        endpoints: Vec<Endpoint>,
        unavailable: Vec<Unavailable>,
        default_model: &str,
    ) -> Result<Self, Error> {
        if endpoints.is_empty() {
            return Err(Error::NoProviders);
        }
        let http = chat_completions::http()?;
        let clients = endpoints
            .into_iter()
            .map(|endpoint| Client {
                origin: Origin {
                    id: endpoint.id,
                    name: endpoint.name,
                },
                inner: ChatClient::with_http(http.clone(), endpoint.base_url, endpoint.api_key)
                    .only(endpoint.models),
            })
            .collect();
        let mut providers = Self {
            default: 0,
            clients,
            unavailable,
            http,
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
}

impl Provider for Providers {
    fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
        async move {
            // Everything at once: the catalogue's deadline must not delay the
            // lists, and one slow endpoint must not delay the others' start.
            let ids = self.clients.iter().map(|c| c.inner.ids());
            let (catalog, ids) =
                futures::join!(catalog::fetch(&self.http), futures::future::join_all(ids));
            let catalog = catalog.ok();
            let listed = self
                .clients
                .iter()
                .zip(ids)
                .map(|(client, ids)| {
                    let models = ids.map(|ids| client.inner.describe(ids, catalog.as_ref()));
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
            self.clients[i]
                .inner
                .stream(Request { model, ..request })
                .await
        }
        .boxed()
    }

    fn retry(&self, error: &BoxError) -> Option<Retry> {
        chat_completions::retry(error)
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
