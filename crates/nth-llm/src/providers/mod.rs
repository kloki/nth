//! Several endpoints behind one `Provider`. A model id is `provider/model`:
//! the prefix picks the endpoint, the rest goes on the wire. An id without
//! a known prefix goes whole to the default provider, so sessions saved
//! before there were several keep working. The catalogue then says which
//! protocol the model speaks there, and the endpoint's client for it takes
//! the request.

mod endpoint;
mod listing;

pub use endpoint::{EndpointConfig, Unavailable};
use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
use nth_protocol::{BoxError, Listing, Provider, Request, Retry, StreamEvent};
use tokio::task::JoinHandle;

use self::endpoint::Endpoint;
use crate::{
    catalog::{Snapshot, Wire},
    chat_completions, messages, responses,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no provider has its API key set")]
    NoProviders,
    #[error("{id} is configured but {api_key_env} is not set")]
    Unavailable { id: String, api_key_env: String },
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
}

pub struct Providers {
    /// Index of the endpoint a bare id goes to: the one the config's model
    /// names, else the first.
    default: usize,
    endpoints: Vec<Endpoint>,
    unavailable: Vec<Unavailable>,
    catalog: Snapshot,
    /// The catalogue fetch started with the providers, so the first
    /// request need not wait for it. Aborted when they drop; `Providers`
    /// is not `Clone`, so the handle has one owner.
    fetch: Option<JoinHandle<()>>,
}

impl Providers {
    /// `default_model` picks the default provider; a model of an
    /// unavailable provider is an error here rather than at the first turn.
    pub fn new(
        endpoints: Vec<EndpointConfig>,
        unavailable: Vec<Unavailable>,
        default_model: &str,
    ) -> Result<Self, Error> {
        let http = crate::http::client()?;
        let catalog = Snapshot::new(http.clone());
        let mut providers = Self::build(endpoints, unavailable, default_model, http, catalog)?;
        providers.start_fetch();
        Ok(providers)
    }

    fn build(
        endpoints: Vec<EndpointConfig>,
        unavailable: Vec<Unavailable>,
        default_model: &str,
        http: reqwest::Client,
        catalog: Snapshot,
    ) -> Result<Self, Error> {
        if endpoints.is_empty() {
            return Err(Error::NoProviders);
        }
        let endpoints = endpoints
            .into_iter()
            .map(|config| Endpoint::new(config, http.clone()))
            .collect();
        let mut providers = Self {
            default: 0,
            endpoints,
            unavailable,
            catalog,
            fetch: None,
        };
        providers.default = providers.route(default_model)?.0;
        Ok(providers)
    }

    /// Fetches the catalogue in the background. Without a runtime, as in a
    /// test, the first request fetches it instead.
    fn start_fetch(&mut self) {
        self.fetch = tokio::runtime::Handle::try_current().ok().map(|runtime| {
            let catalog = self.catalog.clone();
            runtime.spawn(async move {
                catalog.get().await;
            })
        });
    }

    /// `model` as `provider/model`, so a bare id shows and compares like the
    /// listed ones. An id this cannot route is returned as it is.
    pub fn qualify(&self, model: &str) -> String {
        match self.route(model) {
            Ok((i, wire)) => format!("{}/{wire}", self.endpoints[i].origin.id),
            Err(_) => model.to_string(),
        }
    }

    /// Whether `model` can run: an id of an unavailable provider cannot.
    pub fn check(&self, model: &str) -> Result<(), Error> {
        self.route(model).map(|_| ())
    }

    /// The endpoint `model` is for, and the id it knows the model by.
    fn route<'a>(&self, model: &'a str) -> Result<(usize, &'a str), Error> {
        if let Some((prefix, wire)) = model.split_once('/') {
            if let Some(i) = self.endpoints.iter().position(|e| e.origin.id == prefix) {
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
            let ids = self.endpoints.iter().map(|e| e.ids());
            let (catalog, ids) = futures::join!(self.catalog.get(), futures::future::join_all(ids));
            let listed = self
                .endpoints
                .iter()
                .zip(ids)
                .map(|(endpoint, ids)| {
                    let models = ids.map(|ids| endpoint.describe(ids, catalog));
                    (endpoint.origin.clone(), models)
                })
                .collect();
            listing::merge(listed).map_err(BoxError::from)
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
            let endpoint = &self.endpoints[i];
            let (wire, known) = endpoint.wire(self.catalog.get().await, model);
            // Whatever picked the effort (the config, a resumed session, a
            // model switch), a known model is sent only a level it takes.
            let request = match known {
                Some(known) => Request {
                    effort: request.effort.nearest(&known.efforts()),
                    ..request
                },
                None => request,
            };
            Ok(match wire {
                Wire::ChatCompletions => endpoint.chat.stream(request).await?,
                Wire::Messages => endpoint.messages.stream(request, known).await?,
                Wire::Responses => endpoint.responses.stream(request, known).await?,
            })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(id: &str) -> EndpointConfig {
        EndpointConfig {
            id: id.into(),
            name: id.to_uppercase(),
            base_url: format!("https://{id}.test/v1"),
            api_key: "key".into(),
            models: Vec::new(),
        }
    }

    fn unavailable() -> Vec<Unavailable> {
        vec![Unavailable {
            id: "muted".into(),
            api_key_env: "MUTED_API_KEY".into(),
        }]
    }

    fn providers(default_model: &str) -> Providers {
        Providers::new(
            vec![endpoint("lyceum"), endpoint("opencode")],
            unavailable(),
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
        let error = Providers::new(vec![endpoint("lyceum")], unavailable(), "muted/glm").err();
        assert!(matches!(error, Some(Error::Unavailable { .. })));
        assert!(matches!(
            Providers::new(Vec::new(), Vec::new(), "x").err(),
            Some(Error::NoProviders)
        ));
    }

    #[tokio::test]
    async fn the_catalogue_picks_the_protocol() {
        let catalog = serde_json::from_str(include_str!("../../tests/fixtures/models_dev.json"))
            .expect("valid fixture");
        let go = EndpointConfig {
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
            let (wire, known) = providers.endpoints[i].wire(providers.catalog.get().await, model);
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

    #[tokio::test]
    async fn dropping_the_providers_stops_the_catalogue_fetch() {
        // A server that accepts the connection and never answers, so the
        // fetch is still running when the providers drop.
        let server = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        let url = format!("http://{}", server.local_addr().expect("bound"));
        let metrics = tokio::runtime::Handle::current().metrics();
        let before = metrics.num_alive_tasks();

        let mut providers = Providers::build(
            vec![endpoint("lyceum")],
            Vec::new(),
            "glm",
            reqwest::Client::new(),
            Snapshot::at(url),
        )
        .expect("one provider");
        providers.start_fetch();
        assert_eq!(metrics.num_alive_tasks(), before + 1, "the fetch runs");

        drop(providers);
        tokio::task::yield_now().await;
        assert_eq!(metrics.num_alive_tasks(), before, "the fetch is gone");
    }
}
