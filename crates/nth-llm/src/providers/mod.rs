//! Several endpoints behind one `Provider`. A model id is `provider/model`:
//! the prefix picks the endpoint, the rest goes on the wire. An id without
//! a known prefix goes whole to the default provider, so sessions saved
//! before there were several keep working. The catalogue then says which
//! protocol the model speaks there, and the endpoint's client for it takes
//! the request. ChatGPT is always among the endpoints, but runs only while
//! you are connected.

mod chatgpt;
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
    #[error("no provider has its API key set or is connected")]
    NoProviders,
    #[error("{id} is configured but {api_key_env} is not set")]
    Unavailable { id: String, api_key_env: String },
    /// An endpoint that signs in rather than with a key, as ChatGPT,
    /// while it is not connected.
    #[error(transparent)]
    Auth(#[from] nth_auth::Error),
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
        let endpoints: Vec<_> = endpoints
            .into_iter()
            .map(|config| Endpoint::new(config, http.clone()))
            .collect();
        // A bare id goes to the first that can run now.
        let default = endpoints
            .iter()
            .position(|e| e.available().is_ok())
            .ok_or(Error::NoProviders)?;
        let mut providers = Self {
            default,
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

    /// Whether `model` can run: an id of an unavailable provider cannot,
    /// nor one of ChatGPT's while it is not connected.
    pub fn check(&self, model: &str) -> Result<(), Error> {
        self.route(model).map(|_| ())
    }

    /// The endpoint `model` is for, and the id it knows the model by.
    fn route<'a>(&self, model: &'a str) -> Result<(usize, &'a str), Error> {
        if let Some((prefix, wire)) = model.split_once('/') {
            if let Some(i) = self.endpoints.iter().position(|e| e.origin.id == prefix) {
                self.endpoints[i].available()?;
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
            // Every endpoint at once, so one slow one does not delay the
            // others' start. One that cannot run now is left out, not
            // reported: ChatGPT not being connected is no failure.
            let listed = self
                .endpoints
                .iter()
                .filter(|e| e.available().is_ok())
                .map(|e| async { (e.origin.clone(), e.list(&self.catalog).await) });
            listing::merge(futures::future::join_all(listed).await).map_err(BoxError::from)
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
            // A ChatGPT login that cannot refresh fails here, for good.
            let auth = endpoint.auth().await?;
            let catalog = self.catalog.get().await;
            let (wire, known) = endpoint.wire(catalog, model);
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
                Wire::ChatCompletions => {
                    let cache_key = endpoint
                        .takes_cache_key(catalog)
                        .then_some(request.session_id);
                    endpoint.chat.stream(request, cache_key, &auth).await?
                }
                Wire::Messages => endpoint.messages.stream(request, known, &auth).await?,
                Wire::Responses => endpoint.responses.stream(request, known, &auth).await?,
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
    use futures::StreamExt;

    use super::*;
    use crate::Credentials;

    fn endpoint(id: &str) -> EndpointConfig {
        EndpointConfig {
            id: id.into(),
            name: id.to_uppercase(),
            base_url: format!("https://{id}.test/v1"),
            credentials: Credentials::Key("key".into()),
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
            (Wire::ChatCompletions, Some(131_072)),
            "an endpoint the catalogue does not know borrows the model, not its wire"
        );
        assert_eq!(wire("lyceum/unlisted").await, (Wire::ChatCompletions, None));
    }

    /// A ChatGPT login kept in `dir`, and its store to connect it with.
    fn chatgpt(dir: &tempfile::TempDir) -> (nth_auth::Store, nth_auth::ChatGpt) {
        let store = nth_auth::Store::at(dir.path().join("auth.json"));
        let chatgpt = nth_auth::ChatGpt::new(store.clone()).expect("a client");
        (store, chatgpt)
    }

    async fn connect(store: &nth_auth::Store) {
        let auth = nth_auth::OAuth {
            access: "access".into(),
            refresh: "refresh".into(),
            expires: u64::MAX,
            account_id: Some("account".into()),
        };
        store
            .save(nth_auth::ChatGpt::ID, &auth)
            .await
            .expect("saved");
    }

    /// OpenAI's entry with one model the subscription serves.
    fn openai() -> Snapshot {
        let catalog = serde_json::json!({
            "openai": {"npm": "@ai-sdk/openai", "models": {"gpt-5.5": {"reasoning": true}}},
        });
        Snapshot::of(serde_json::from_value(catalog).expect("a valid catalogue"))
    }

    #[tokio::test]
    async fn chatgpt_runs_only_while_connected() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (store, chatgpt) = chatgpt(&dir);
        let lyceum = EndpointConfig {
            models: vec!["glm".into()],
            ..endpoint("lyceum")
        };
        let providers = Providers::build(
            vec![lyceum, EndpointConfig::chatgpt(chatgpt)],
            Vec::new(),
            "glm",
            reqwest::Client::new(),
            openai(),
        )
        .expect("lyceum runs");
        let ids = async || -> Vec<String> {
            let listing = providers.models().await.expect("listed");
            assert_eq!(listing.failed, [], "not connected is no failure");
            listing.models.into_iter().map(|m| m.id).collect()
        };

        let error = providers
            .check("chatgpt/gpt-5.5")
            .expect_err("not connected");
        assert_eq!(
            error.to_string(),
            "ChatGPT is not connected: run /connect or `nth connect`"
        );
        assert_eq!(ids().await, ["lyceum/glm"]);
        let request = Request {
            model: "chatgpt/gpt-5.5",
            session_id: "session",
            parent_session_id: None,
            effort: Default::default(),
            messages: &[],
            tools: &[],
        };
        let error = providers.stream(request).await.err().expect("refused");
        assert_eq!(providers.retry(&error), None, "connecting takes you");

        connect(&store).await;
        assert_eq!(
            providers.route("chatgpt/gpt-5.5").ok(),
            Some((1, "gpt-5.5"))
        );
        assert_eq!(ids().await, ["lyceum/glm", "chatgpt/gpt-5.5"]);
        assert_eq!(
            providers.route("glm").ok(),
            Some((0, "glm")),
            "still lyceum"
        );
    }

    #[tokio::test]
    async fn chatgpt_alone_runs_once_connected() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (store, chatgpt) = chatgpt(&dir);
        let build = || {
            Providers::build(
                vec![EndpointConfig::chatgpt(chatgpt.clone())],
                Vec::new(),
                "",
                reqwest::Client::new(),
                openai(),
            )
        };
        assert!(matches!(build().err(), Some(Error::NoProviders)));

        connect(&store).await;
        let providers = build().expect("connected");
        assert_eq!(providers.qualify("gpt-5.5"), "chatgpt/gpt-5.5");
    }

    #[tokio::test]
    async fn chatgpt_requests_carry_the_account_and_the_session() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let (store, chatgpt) = chatgpt(&dir);
        connect(&store).await;
        let (url, head) =
            crate::testing::serve_once(include_bytes!("../../tests/fixtures/zen_responses.sse"))
                .await;
        let codex = EndpointConfig {
            base_url: url,
            ..EndpointConfig::chatgpt(chatgpt)
        };
        let providers = Providers::build(
            vec![codex],
            Vec::new(),
            "chatgpt/gpt-5.5",
            reqwest::Client::new(),
            openai(),
        )
        .expect("connected");

        let request = Request {
            model: "chatgpt/gpt-5.5",
            session_id: "ses_1",
            parent_session_id: None,
            effort: Default::default(),
            messages: &[],
            tools: &[],
        };
        let events: Vec<_> = providers
            .stream(request)
            .await
            .expect("streams")
            .collect()
            .await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");

        let (head, _) = head.await.expect("a request");
        let head = head.to_lowercase();
        let lines: Vec<_> = head.lines().collect();
        assert_eq!(lines[0], "post /responses http/1.1", "over responses");
        for header in [
            "authorization: bearer access",
            "chatgpt-account-id: account",
            "originator: nth",
            "session-id: ses_1",
        ] {
            assert!(lines.contains(&header), "{header} in {head}");
        }
    }

    /// The head, lowercased, and the body of the one request `providers`
    /// sends for `model` on behalf of a subagent session of `parent`.
    async fn sent(
        id: &str,
        catalog: Snapshot,
        model: &str,
        parent: Option<&str>,
    ) -> (String, serde_json::Value) {
        let (url, sent) =
            crate::testing::serve_once(include_bytes!("../../tests/fixtures/go_stream.sse")).await;
        let providers = Providers::build(
            vec![EndpointConfig {
                base_url: url,
                ..endpoint(id)
            }],
            Vec::new(),
            model,
            reqwest::Client::new(),
            catalog,
        )
        .expect("one provider");
        let request = Request {
            model,
            session_id: "ses_1",
            parent_session_id: parent,
            effort: Default::default(),
            messages: &[],
            tools: &[],
        };
        let events: Vec<_> = providers
            .stream(request)
            .await
            .expect("streams")
            .collect()
            .await;
        assert!(events.iter().all(Result::is_ok), "{events:?}");
        let (head, body) = sent.await.expect("a request");
        let body = serde_json::from_str(&body).expect("a json body");
        (head.to_lowercase(), body)
    }

    /// A catalogue of one provider, `id`, speaking chat completions.
    fn compatible(id: &str) -> Snapshot {
        let catalog = serde_json::json!({
            id: {"npm": "@ai-sdk/openai-compatible", "models": {"kimi-k3": {}}},
        });
        Snapshot::of(serde_json::from_value(catalog).expect("a valid catalogue"))
    }

    #[tokio::test]
    async fn requests_carry_the_session_and_its_parent() {
        let (head, _) = sent(
            "lyceum",
            compatible("other"),
            "lyceum/kimi-k3",
            Some("ses_0"),
        )
        .await;
        let lines: Vec<_> = head.lines().collect();
        for header in [
            "x-opencode-session: ses_1",
            "x-opencode-session-id: ses_1",
            "x-opencode-parent-session-id: ses_0",
        ] {
            assert!(lines.contains(&header), "{header} in {head}");
        }

        let (head, _) = sent("lyceum", compatible("other"), "lyceum/kimi-k3", None).await;
        assert!(head.contains("x-opencode-session-id: ses_1"), "{head}");
        assert!(!head.contains("x-opencode-parent-session-id"), "{head}");
    }

    #[tokio::test]
    async fn only_a_provider_known_to_take_it_gets_a_cache_key() {
        let (_, body) = sent("moonshotai", compatible("moonshotai"), "kimi-k3", None).await;
        assert_eq!(body["prompt_cache_key"], "ses_1");

        let (_, body) = sent("lyceum", compatible("lyceum"), "kimi-k3", None).await;
        assert!(body.get("prompt_cache_key").is_none(), "{body}");
        let (_, body) = sent("moonshotai", compatible("other"), "kimi-k3", None).await;
        assert!(
            body.get("prompt_cache_key").is_none(),
            "an endpoint the catalogue does not know gets none"
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
