//! The models.dev catalogue: what is known about every provider's models,
//! their protocol and their limits. Fetched until it loads, with backoff
//! between failures and the last fetched copy on disk served meanwhile,
//! then shared: the listing reads names and limits from it, and a request
//! the protocol to speak.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use nth_protocol::{BoxError, Cost, Effort};
use serde::Deserialize;
use tokio::time::Instant;

const URL: &str = "https://models.dev/api.json";
/// The catalogue only refines a listing, so a stall must not hold it up.
const TIMEOUT: Duration = Duration::from_secs(5);
/// How long after a failed fetch the next may run, doubling with each
/// failure up to `RETRY_MAX`.
const RETRY_INITIAL: Duration = Duration::from_secs(5);
const RETRY_MAX: Duration = Duration::from_secs(300);

/// Providers by their models.dev id.
pub(crate) type Catalog = HashMap<String, Provider>;

#[derive(Deserialize)]
pub(crate) struct Provider {
    pub(crate) npm: Option<String>,
    pub(crate) api: Option<String>,
    #[serde(default)]
    pub(crate) models: HashMap<String, Model>,
}

#[derive(Deserialize, Default)]
pub(crate) struct Model {
    pub(crate) name: Option<String>,
    pub(crate) reasoning: Option<bool>,
    /// How its reasoning is steered; empty when it cannot be.
    #[serde(default, deserialize_with = "lenient")]
    pub(crate) reasoning_options: Option<Vec<ReasoningOption>>,
    pub(crate) limit: Option<Limit>,
    /// Left out when not shaped as nth expects, as the reasoning options.
    #[serde(default, deserialize_with = "lenient")]
    pub(crate) cost: Option<Cost>,
    /// Set when this model needs a different SDK, and so a different wire
    /// protocol, than its provider's default.
    pub(crate) provider: Option<Override>,
    /// `provider/model` of the entry for the maker's own, as
    /// `zhipuai/glm-5.3` for openrouter's `z-ai/glm-5.3`.
    pub(crate) canonical_model_id: Option<String>,
    /// What it takes and gives besides text.
    pub(crate) modalities: Option<Modalities>,
}

#[derive(Deserialize, Default)]
pub(crate) struct Modalities {
    #[serde(default)]
    pub(crate) input: Vec<String>,
}

/// One way a model's reasoning can be steered.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ReasoningOption {
    /// The effort levels it takes; null is not thinking at all.
    Effort { values: Vec<Option<String>> },
    /// A thinking budget in tokens, within these bounds. Some entries give
    /// a negative bound, which bounds nothing.
    BudgetTokens { min: Option<i64>, max: Option<i64> },
    /// Thinking on or off, with no level between.
    Toggle,
    /// A kind nth does not steer by, so a new one does not break the
    /// catalogue.
    #[serde(other)]
    Other,
}

/// The efforts a model is offered when nothing narrows them: the ones a
/// thinking budget is picked for, and what an unknown model may take.
pub(crate) const BUDGET_EFFORTS: [Effort; 3] = [Effort::Low, Effort::Medium, Effort::High];

impl Model {
    /// Whether it takes images. The catalogue's `attachment` flag disagrees
    /// with its modalities for hundreds of models, so only these count.
    pub(crate) fn takes_images(&self) -> bool {
        self.modalities
            .as_ref()
            .is_some_and(|m| m.input.iter().any(|i| i == "image"))
    }

    /// The efforts it takes, least first. A budget is steered by the levels
    /// nth has budgets for. A toggle alone, or no option, offers none, as
    /// does a model that does not reason; one the catalogue says reasons
    /// but gives no options for is offered the budget levels.
    pub(crate) fn efforts(&self) -> Vec<Effort> {
        let Some(options) = &self.reasoning_options else {
            return if self.reasoning == Some(true) {
                BUDGET_EFFORTS.to_vec()
            } else {
                Vec::new()
            };
        };
        if let Some(values) = options.iter().find_map(|o| match o {
            ReasoningOption::Effort { values } => Some(values),
            _ => None,
        }) {
            let mut efforts: Vec<Effort> = values
                .iter()
                .filter_map(|v| Effort::from_name(v.as_deref().unwrap_or("none")))
                .collect();
            efforts.sort_by_key(|e| Effort::ALL.iter().position(|a| a == e));
            return efforts;
        }
        if self.budget().is_some() {
            return BUDGET_EFFORTS.to_vec();
        }
        Vec::new()
    }

    /// Whether it takes effort levels rather than only a budget.
    pub(crate) fn takes_effort(&self) -> bool {
        self.reasoning_options
            .iter()
            .flatten()
            .any(|o| matches!(o, ReasoningOption::Effort { .. }))
    }

    /// The bounds of its thinking budget, when it takes one.
    pub(crate) fn budget(&self) -> Option<(Option<u64>, Option<u64>)> {
        self.reasoning_options
            .iter()
            .flatten()
            .find_map(|o| match o {
                ReasoningOption::BudgetTokens { min, max } => Some((
                    min.and_then(|m| u64::try_from(m).ok()),
                    max.and_then(|m| u64::try_from(m).ok()),
                )),
                _ => None,
            })
    }
}

/// A field, or none when it is not shaped as nth expects: one odd entry
/// must not cost the whole catalogue.
fn lenient<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).ok())
}

#[derive(Deserialize, Default)]
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
    Responses,
}

impl Wire {
    fn from_npm(npm: &str) -> Option<Self> {
        match npm {
            "@ai-sdk/openai-compatible" => Some(Self::ChatCompletions),
            "@ai-sdk/anthropic" => Some(Self::Messages),
            "@ai-sdk/openai" => Some(Self::Responses),
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

/// The catalogue, fetched by the providers' background task (`refresh`),
/// or by a request that needs it before that has one, and then kept for
/// the life of the process. Every fetched catalogue is kept on disk, and
/// that copy is served before the network is asked: stale, it still routes
/// models right, and a request never waits on a network that hangs while
/// it is there. `refresh` replaces it with a fetched one. Without either, a
/// failed fetch leaves no catalogue, under which every model speaks chat
/// completions as with an unknown endpoint, and is tried again after
/// `backoff`: a blip at start-up must not route Claude over the wrong
/// protocol for the whole run.
#[derive(Clone)]
pub(crate) struct Snapshot {
    http: reqwest::Client,
    /// Where it is fetched from; none for one that never fetches.
    url: Option<String>,
    /// Where the last fetched catalogue is kept; none to keep none.
    cache: Option<PathBuf>,
    state: Arc<Mutex<State>>,
    /// Held while a fetch runs, so callers that find none wait on that one
    /// rather than start their own.
    fetching: Arc<tokio::sync::Mutex<()>>,
}

#[derive(Default)]
struct State {
    catalog: Option<Arc<Catalog>>,
    /// Whether `catalog` is the copy on disk, which the network is still
    /// tried for, rather than a fetched one.
    cached: bool,
    /// When the last fetch failed, on tokio's clock so tests can move it.
    failed_at: Option<Instant>,
    /// Failed fetches since the last success.
    attempts: u32,
}

impl State {
    /// Whether another fetch may run: none has failed yet, or the backoff
    /// for the failures so far has passed.
    fn due(&self) -> bool {
        self.next_try().is_some_and(|wait| wait.is_zero())
    }

    /// How long until the network is to be tried again: none once a
    /// catalogue has been fetched, else what is left of the backoff.
    fn next_try(&self) -> Option<Duration> {
        if self.catalog.is_some() && !self.cached {
            return None;
        }
        Some(self.failed_at.map_or(Duration::ZERO, |at| {
            backoff(self.attempts).saturating_sub(at.elapsed())
        }))
    }
}

/// How long to wait after `attempts` failed fetches before the next:
/// `RETRY_INITIAL`, doubling, at most `RETRY_MAX`.
fn backoff(attempts: u32) -> Duration {
    let doublings = attempts.saturating_sub(1).min(31);
    RETRY_INITIAL.saturating_mul(1 << doublings).min(RETRY_MAX)
}

impl Snapshot {
    pub(crate) fn new(http: reqwest::Client, cache: Option<PathBuf>) -> Self {
        Self {
            http,
            url: Some(URL.into()),
            cache,
            state: Arc::default(),
            fetching: Arc::default(),
        }
    }

    /// The catalogue: at once when one is served, fetched or cached; else,
    /// when a fetch is due, the copy on disk, else a fetch, waiting for
    /// whatever is under way first; else none, without waiting. So with
    /// nothing on disk, the request or listing that first asks once the
    /// backoff has run out waits on the retry, at most `TIMEOUT`, as does
    /// any caller that asks while it runs: without the catalogue a model
    /// may go over the wrong protocol, which is worse than a short wait. A
    /// cached catalogue is never retried here, so no caller waits on the
    /// network while one serves; `refresh` retries it.
    pub(crate) async fn get(&self) -> Option<Arc<Catalog>> {
        let url = self.url.as_deref()?;
        if let Some(found) = self.settled() {
            return found;
        }
        let _one = self.fetching.lock().await;
        // The fetch this waited on may have loaded one, or failed and so
        // put the next off.
        if let Some(found) = self.settled() {
            return found;
        }
        if let Some(cached) = self.load_cache().await {
            return Some(cached);
        }
        self.fetch(url).await
    }

    /// Serves the copy on disk, then fetches until a fetch succeeds, each
    /// try once the backoff allows: what the providers' background task
    /// runs, so the catalogue loads before a request asks for it and a
    /// cached one is replaced without a request waiting on the network.
    pub(crate) async fn refresh(&self) {
        let Some(url) = self.url.as_deref() else {
            return;
        };
        {
            // Held while reading, so a request that comes meanwhile waits
            // on the disk rather than going to the network.
            let _one = self.fetching.lock().await;
            if self.state().catalog.is_none() {
                self.load_cache().await;
            }
        }
        loop {
            let Some(wait) = self.state().next_try() else {
                return;
            };
            tokio::time::sleep(wait).await;
            let _one = self.fetching.lock().await;
            // A caller may have fetched while this slept or waited.
            if self.state().due() {
                self.fetch(url).await;
            }
        }
    }

    /// One fetch from `url`, the caller holding `fetching`. A success is
    /// served and kept on disk; a failure serves what was served before.
    async fn fetch(&self, url: &str) -> Option<Arc<Catalog>> {
        // A caller dropped mid-fetch records no failure, deliberately: the
        // next caller simply fetches again.
        match fetch(&self.http, url).await {
            Ok((catalog, body)) => {
                let catalog = Arc::new(catalog);
                *self.state() = State {
                    catalog: Some(catalog.clone()),
                    ..State::default()
                };
                if let Some(cache) = &self.cache {
                    write_cache(cache.clone(), body).await;
                }
                Some(catalog)
            }
            Err(_) => {
                let mut state = self.state();
                state.failed_at = Some(Instant::now());
                state.attempts = state.attempts.saturating_add(1);
                state.catalog.clone()
            }
        }
    }

    /// The copy on disk, served, the caller holding `fetching` with nothing
    /// served yet. Read, then served in one step, so a caller dropped
    /// mid-read leaves everything as it was. The backoff stays as it is:
    /// with no fetch failed yet, `refresh` fetches at once.
    async fn load_cache(&self) -> Option<Arc<Catalog>> {
        let cached = Arc::new(read_cache(self.cache.clone()?).await?);
        let mut state = self.state();
        state.catalog = Some(cached.clone());
        state.cached = true;
        Some(cached)
    }

    /// The answer without a fetch, when there is one: the catalogue served,
    /// or none while the backoff runs.
    fn settled(&self) -> Option<Option<Arc<Catalog>>> {
        let state = self.state();
        match &state.catalog {
            Some(catalog) => Some(Some(catalog.clone())),
            None if !state.due() => Some(None),
            None => None,
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("catalogue state lock poisoned")
    }

    /// None at all, and nothing ever fetches or reads one: every model
    /// speaks chat completions, as after a failed fetch.
    pub(crate) fn none(http: reqwest::Client) -> Self {
        Self {
            url: None,
            ..Self::new(http, None)
        }
    }

    #[cfg(test)]
    pub(crate) fn of(catalog: Catalog) -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                catalog: Some(Arc::new(catalog)),
                ..State::default()
            })),
            ..Self::new(reqwest::Client::new(), None)
        }
    }

    /// One fetched from `url` instead of models.dev, kept at `cache`.
    #[cfg(test)]
    pub(crate) fn at(url: String, cache: Option<PathBuf>) -> Self {
        Self {
            url: Some(url),
            ..Self::new(reqwest::Client::new(), cache)
        }
    }
}

/// The catalogue at `url`, with the body it came in to keep on disk.
async fn fetch(http: &reqwest::Client, url: &str) -> Result<(Catalog, Bytes), BoxError> {
    let body = http
        .get(url)
        .timeout(TIMEOUT)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    Ok((serde_json::from_slice(&body)?, body))
}

/// The catalogue kept at `path`; none when it is missing or unreadable, as
/// for a run that never fetched one.
async fn read_cache(path: PathBuf) -> Option<Catalog> {
    tokio::task::spawn_blocking(move || serde_json::from_slice(&std::fs::read(path).ok()?).ok())
        .await
        .ok()
        .flatten()
}

/// Keeps `body` at `path`, written aside and renamed over so another nth
/// starting beside never reads half a catalogue. Its errors are dropped:
/// the copy only helps a later run that cannot fetch, and failing to keep
/// it must not cost this run the catalogue it has.
async fn write_cache(path: PathBuf, body: Bytes) {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    // Unique per write, as two nth may keep the catalogue at once.
    let n = WRITES.fetch_add(1, Ordering::Relaxed);
    let _ = tokio::task::spawn_blocking(move || {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension(format!("{}.{n}.tmp", std::process::id()));
        let kept = std::fs::write(&tmp, &body).and_then(|()| std::fs::rename(&tmp, &path));
        if kept.is_err() {
            // A full disk or a folder gone mid-write leaves no stray half.
            let _ = std::fs::remove_file(&tmp);
        }
        kept
    })
    .await;
}
/// The catalogue's entry for the provider serving `base_url`, else the one
/// named `id`: some entries, as OpenAI's, give no `api` to match. With the
/// id the catalogue knows it by.
pub(crate) fn provider_for<'a>(
    catalog: &'a Catalog,
    id: &str,
    base_url: &str,
) -> Option<(&'a str, &'a Provider)> {
    let base_url = endpoint_identity(base_url);
    catalog
        .iter()
        .find(|(_, p)| p.api.as_deref().map(endpoint_identity) == Some(base_url))
        .or_else(|| catalog.get_key_value(id))
        .map(|(id, p)| (id.as_str(), p))
}

/// OpenCode renamed its gateway routes. Compare both generations as the
/// same endpoint without changing where a configured client sends requests.
fn endpoint_identity(url: &str) -> &str {
    match url.trim_end_matches('/') {
        "https://opencode.ai/zen/v1" => "https://opencode.ai/inference/openai/v1",
        "https://opencode.ai/zen/go/v1" => "https://opencode.ai/inference/go/openai/v1",
        url => url,
    }
}

/// What another provider's entry says about `model`, for an endpoint the
/// catalogue has none for: the model of that id, else of the part after its
/// last `/`. Of several, the canonical entry one of them names, else the
/// first by provider id.
pub(crate) fn borrowed<'a>(catalog: &'a Catalog, model: &str) -> Option<&'a Model> {
    let mut providers: Vec<_> = catalog.iter().collect();
    providers.sort_by_key(|(id, _)| *id);
    let named = |name: &str| -> Vec<&'a Model> {
        providers
            .iter()
            .filter_map(|(_, p)| p.models.get(name))
            .collect()
    };
    let mut found = named(model);
    if found.is_empty()
        && let Some((_, bare)) = model.rsplit_once('/')
    {
        found = named(bare);
    }
    found
        .iter()
        .find_map(|m| {
            let (provider, id) = m.canonical_model_id.as_deref()?.split_once('/')?;
            catalog.get(provider)?.models.get(id)
        })
        .or(found.first().copied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    const FIXTURE: &str = include_str!("../tests/fixtures/models_dev.json");

    #[test]
    fn the_backoff_doubles_up_to_its_cap() {
        let secs = |attempts| backoff(attempts).as_secs();
        assert_eq!(secs(1), 5);
        assert_eq!(secs(2), 10);
        assert_eq!(secs(3), 20);
        assert_eq!(secs(7), 300, "320 is capped");
        assert_eq!(secs(u32::MAX), 300);
    }

    #[tokio::test]
    async fn a_failed_fetch_is_tried_again_after_the_backoff() {
        use std::sync::atomic::Ordering::SeqCst;

        let (url, answered) =
            testing::serve_many(vec![(500, b"{}"), (500, b"{}"), (200, FIXTURE.as_bytes())]).await;
        let snapshot = Snapshot::at(url, None);

        assert!(snapshot.get().await.is_none(), "the fetch failed");
        assert_eq!(answered.load(SeqCst), 1);

        // The clock stands still while nothing goes over the network, and
        // runs while a fetch does, so a slow one cannot fire its timeout.
        tokio::time::pause();
        tokio::time::advance(RETRY_INITIAL - Duration::from_secs(1)).await;
        assert!(snapshot.get().await.is_none(), "still backing off");
        assert_eq!(answered.load(SeqCst), 1, "without a fetch");

        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::time::resume();
        assert!(snapshot.get().await.is_none(), "the second fetch failed");
        assert_eq!(answered.load(SeqCst), 2);

        tokio::time::pause();
        tokio::time::advance(RETRY_INITIAL).await;
        assert!(snapshot.get().await.is_none(), "the wait doubled");
        assert_eq!(answered.load(SeqCst), 2);

        tokio::time::advance(RETRY_INITIAL).await;
        tokio::time::resume();
        let catalog = snapshot.get().await.expect("the third fetch loaded it");
        assert!(catalog.contains_key("opencode-go"));
        assert_eq!(answered.load(SeqCst), 3);

        assert!(snapshot.get().await.is_some(), "kept");
        assert_eq!(answered.load(SeqCst), 3, "without another fetch");
    }

    #[tokio::test]
    async fn callers_during_a_fetch_wait_on_it_rather_than_start_their_own() {
        use std::sync::atomic::Ordering::SeqCst;

        let (url, answered) = testing::serve_many(vec![(500, b"{}")]).await;
        let snapshot = Snapshot::at(url, None);
        let (a, b) = futures::join!(snapshot.get(), snapshot.get());
        assert!(a.is_none() && b.is_none());
        assert_eq!(answered.load(SeqCst), 1, "one fetch for both");
    }

    #[tokio::test]
    async fn an_offline_snapshot_never_fetches() {
        // No server at all: a fetch would fail, and `get` would wait on it.
        let snapshot = Snapshot::none(reqwest::Client::new());
        assert!(snapshot.get().await.is_none());
        snapshot.refresh().await;
        assert!(snapshot.state().failed_at.is_none(), "nothing was tried");
    }

    const CACHED: &[u8] = br#"{"cached": {"npm": "@ai-sdk/anthropic", "models": {}}}"#;

    /// A URL nothing listens on, so a fetch fails at once.
    fn refused() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a free port");
        format!("http://{}", listener.local_addr().expect("bound"))
    }

    #[tokio::test]
    async fn a_fetched_catalogue_is_kept_on_disk_for_a_run_that_cannot_fetch() {
        let dir = tempfile::tempdir().expect("tempdir");
        // The data directory need not exist yet.
        let cache = dir.path().join("nth").join("models.json");
        let (url, _) = testing::serve_many(vec![(200, FIXTURE.as_bytes())]).await;

        let online = Snapshot::at(url, Some(cache.clone()));
        assert!(online.get().await.is_some());
        assert_eq!(std::fs::read(&cache).expect("kept"), FIXTURE.as_bytes());
        let files: Vec<_> = std::fs::read_dir(cache.parent().expect("a folder"))
            .expect("listed")
            .map(|e| e.expect("an entry").file_name())
            .collect();
        assert_eq!(files, ["models.json"], "nothing left aside");

        let offline = Snapshot::at(refused(), Some(cache));
        let catalog = offline.get().await.expect("read back");
        assert!(catalog.contains_key("opencode-go"));
    }

    /// A cache file holding `CACHED`, in `dir`.
    fn cached(dir: &tempfile::TempDir) -> PathBuf {
        let cache = dir.path().join("models.json");
        std::fs::write(&cache, CACHED).expect("writes");
        cache
    }

    /// Moves the last failure back past its backoff, so the next fetch is
    /// due without the clock paused: a fetch goes over the network, where
    /// a paused clock would jump to reqwest's timeout.
    fn rewind(snapshot: &Snapshot) {
        let mut state = snapshot.state();
        let wait = backoff(state.attempts);
        state.failed_at = state.failed_at.and_then(|at| at.checked_sub(wait));
    }

    #[tokio::test]
    async fn a_cached_catalogue_is_served_before_the_network_answers() {
        // Well under the fetch's 5 s timeout, which a request would wait
        // out on a network that hangs.
        const AT_ONCE: Duration = Duration::from_secs(1);
        let dir = tempfile::tempdir().expect("tempdir");

        let asked = Snapshot::at(testing::serve_silent().await, Some(cached(&dir)));
        let catalog = tokio::time::timeout(AT_ONCE, asked.get())
            .await
            .expect("without waiting on the network")
            .expect("the cached one");
        assert!(catalog.contains_key("cached"));

        // The background task serves it before its own fetch, so a request
        // that comes while that hangs need not wait on it.
        let refreshed = Snapshot::at(testing::serve_silent().await, Some(cached(&dir)));
        let refreshing = refreshed.refresh();
        tokio::pin!(refreshing);
        tokio::select! {
            () = &mut refreshing => panic!("the fetch never answers"),
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
        let catalog = tokio::time::timeout(AT_ONCE, async {
            tokio::select! {
                () = &mut refreshing => panic!("the fetch never answers"),
                catalog = refreshed.get() => catalog,
            }
        })
        .await
        .expect("without waiting on the network")
        .expect("the cached one");
        assert!(catalog.contains_key("cached"));
    }

    #[tokio::test]
    async fn while_a_cached_catalogue_serves_no_request_fetches() {
        use std::sync::atomic::Ordering::SeqCst;

        let dir = tempfile::tempdir().expect("tempdir");
        let (url, answered) = testing::serve_many(vec![(500, b"{}")]).await;
        let snapshot = Snapshot::at(url.clone(), Some(cached(&dir)));

        assert!(snapshot.get().await.expect("cached").contains_key("cached"));
        assert!(
            snapshot.fetch(&url).await.is_some(),
            "a failed retry keeps it"
        );
        assert_eq!(answered.load(SeqCst), 1);

        // Due again, yet served at once: the retry is `refresh`'s. Nothing
        // goes over the network here, so the clock can stand still.
        tokio::time::pause();
        tokio::time::advance(RETRY_MAX).await;
        assert!(snapshot.state().due());
        let catalog = snapshot.get().await.expect("still the cached one");
        assert!(catalog.contains_key("cached"));
        assert_eq!(answered.load(SeqCst), 1, "without a fetch");
    }

    #[tokio::test]
    async fn a_cached_catalogue_is_replaced_by_a_fetch_in_the_background() {
        use std::sync::atomic::Ordering::SeqCst;

        let dir = tempfile::tempdir().expect("tempdir");
        let cache = cached(&dir);
        let (url, answered) =
            testing::serve_many(vec![(500, b"{}"), (200, FIXTURE.as_bytes())]).await;
        let snapshot = Snapshot::at(url.clone(), Some(cache.clone()));
        assert!(snapshot.get().await.expect("cached").contains_key("cached"));
        assert!(snapshot.fetch(&url).await.is_some(), "a failed retry");

        rewind(&snapshot);
        snapshot.refresh().await;

        assert_eq!(answered.load(SeqCst), 2);
        let catalog = snapshot.get().await.expect("fetched");
        assert!(catalog.contains_key("opencode-go"));
        assert!(!snapshot.state().cached);
        assert_eq!(std::fs::read(&cache).expect("kept"), FIXTURE.as_bytes());
    }

    #[tokio::test]
    async fn without_a_cache_refresh_keeps_fetching_until_one_succeeds() {
        use std::sync::atomic::Ordering::SeqCst;

        let (url, answered) =
            testing::serve_many(vec![(500, b"{}"), (200, FIXTURE.as_bytes())]).await;
        let snapshot = Snapshot::at(url, None);
        let refreshing = snapshot.refresh();
        tokio::pin!(refreshing);

        // Driven until the first fetch has failed; it then sleeps out the
        // backoff with nothing on the network, so the clock may jump it.
        while snapshot.state().failed_at.is_none() {
            tokio::select! {
                () = &mut refreshing => panic!("gave up after a failed fetch"),
                () = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
        tokio::time::pause();
        tokio::time::advance(RETRY_INITIAL).await;
        tokio::time::resume();
        refreshing.await;

        assert_eq!(answered.load(SeqCst), 2);
        let catalog = snapshot.get().await.expect("the second fetch loaded it");
        assert!(catalog.contains_key("opencode-go"));
    }

    #[tokio::test]
    async fn refresh_skips_a_retry_a_request_just_made() {
        use std::sync::atomic::Ordering::SeqCst;

        let (url, answered) = testing::serve_many(vec![(500, b"{}")]).await;
        let snapshot = Snapshot::at(url.clone(), None);
        assert!(snapshot.get().await.is_none());
        let refreshing = snapshot.refresh();
        tokio::pin!(refreshing);
        assert!(futures::poll!(&mut refreshing).is_pending(), "backing off");

        // A request takes the fetch as the backoff runs out, so `refresh`,
        // finding one due, waits on it; the request's fails, which puts the
        // next off. The clock stands still only while nothing is on the
        // network.
        let request = snapshot.fetching.clone().lock_owned().await;
        tokio::time::pause();
        tokio::time::advance(RETRY_INITIAL).await;
        tokio::time::resume();
        assert!(futures::poll!(&mut refreshing).is_pending(), "waiting");
        assert!(snapshot.fetch(&url).await.is_none());
        assert_eq!(answered.load(SeqCst), 2);
        drop(request);

        tokio::select! {
            () = &mut refreshing => panic!("gave up while backing off"),
            () = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
        assert_eq!(answered.load(SeqCst), 2, "no second fetch in the backoff");
    }

    #[tokio::test]
    async fn an_unreadable_cache_serves_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join("models.json");
        std::fs::write(&cache, "{ half").expect("writes");
        let snapshot = Snapshot::at(refused(), Some(cache));
        assert!(snapshot.get().await.is_none());
        assert!(!snapshot.state().cached);
    }

    #[test]
    fn the_sdk_picks_the_wire() {
        let catalog: Catalog = serde_json::from_str(FIXTURE).expect("valid fixture");
        let (_, go) =
            provider_for(&catalog, "opencode", "https://opencode.ai/zen/go/v1").expect("go entry");
        assert_eq!(go.wire("glm-5.3"), Some(Wire::ChatCompletions));
        assert_eq!(go.wire("minimax-m3"), Some(Wire::Messages));
        assert_eq!(go.wire("grok-4.7"), Some(Wire::Responses));
        assert_eq!(go.wire("gemini-3.5-flash"), None, "google's is not spoken");
        assert_eq!(go.wire("unlisted"), Some(Wire::ChatCompletions));

        let other = &catalog["other"];
        assert_eq!(
            other.wire("claude-x"),
            Some(Wire::Messages),
            "the provider's SDK"
        );
        assert_eq!(other.wire("unlisted"), Some(Wire::ChatCompletions));
    }

    #[test]
    fn a_provider_is_found_by_its_url_else_by_its_id() {
        let catalog: Catalog = serde_json::from_str(FIXTURE).expect("valid fixture");
        let id = |base_url| provider_for(&catalog, "openai", base_url).map(|(id, _)| id);

        assert_eq!(
            id("https://api.openai.com/v1"),
            Some("openai"),
            "no api, so by id"
        );
        let (_, openai) =
            provider_for(&catalog, "openai", "https://api.openai.com/v1").expect("openai");
        assert_eq!(openai.wire("gpt-x"), Some(Wire::Responses));
        assert_eq!(
            id("https://opencode.ai/zen/go/v1/"),
            Some("opencode-go"),
            "the url wins"
        );
        assert!(provider_for(&catalog, "lyceum", "https://api.lyceum.technology").is_none());
    }

    #[test]
    fn legacy_and_current_gateway_urls_find_the_same_provider() {
        // Both catalogue generations must work with both config generations.
        for (zen, go) in [
            (
                "https://opencode.ai/zen/v1",
                "https://opencode.ai/zen/go/v1",
            ),
            (
                "https://opencode.ai/inference/openai/v1",
                "https://opencode.ai/inference/go/openai/v1",
            ),
        ] {
            let catalog: Catalog = serde_json::from_value(serde_json::json!({
                "opencode": {"api": zen, "models": {}},
                "opencode-go": {"api": go, "models": {}},
            }))
            .expect("valid catalogue");
            for (url, expected) in [
                ("https://opencode.ai/zen/v1", "opencode"),
                ("https://opencode.ai/inference/openai/v1", "opencode"),
                ("https://opencode.ai/zen/go/v1", "opencode-go"),
                ("https://opencode.ai/inference/go/openai/v1", "opencode-go"),
            ] {
                for id in ["zen", "custom", "opencode"] {
                    for url in [url.to_string(), format!("{url}/")] {
                        assert_eq!(
                            provider_for(&catalog, id, &url).map(|(id, _)| id),
                            Some(expected),
                            "{id} at {url}, catalogue {zen} / {go}"
                        );
                    }
                }
            }
            for url in [
                "http://opencode.ai/zen/v1",
                "https://example.com/zen/v1",
                "https://opencode.ai/zen/v1/proxy",
            ] {
                assert!(provider_for(&catalog, "custom", url).is_none());
            }
        }
    }

    #[test]
    fn a_model_is_borrowed_by_id_else_by_bare_name_preferring_the_canonical() {
        let catalog: Catalog = serde_json::from_str(FIXTURE).expect("valid fixture");
        let name = |model| borrowed(&catalog, model).and_then(|m| m.name.as_deref());

        assert_eq!(
            name("z-ai/glm-5.3"),
            Some("GLM-5.3"),
            "openrouter's names zhipuai's"
        );
        assert_eq!(
            borrowed(&catalog, "z-ai/glm-5.3").and_then(|m| m.limit.as_ref()?.context),
            Some(204_800)
        );
        assert_eq!(name("acme/kimi-k3"), Some("Kimi K3"), "by the bare name");
        assert_eq!(name("acme/orphan"), Some("Orphan"), "its canonical is gone");
        assert_eq!(name("claude-x"), Some("Claude X"));
        assert_eq!(name("acme/unknown"), None);
    }

    #[test]
    fn reasoning_options_say_which_efforts_a_model_takes() {
        use Effort::*;
        let catalog: Catalog = serde_json::from_str(FIXTURE).expect("valid fixture");
        let go = &catalog["opencode-go"].models;
        let other = &catalog["other"].models;
        assert_eq!(go["glm-5.3"].efforts(), [Low, High, Max]);
        assert_eq!(go["kimi-k3"].efforts(), [Max]);
        assert_eq!(go["minimax-m3"].efforts(), [], "a toggle has no levels");
        assert_eq!(go["gemini-3.5-flash"].efforts(), [], "does not reason");
        assert_eq!(other["claude-x"].efforts(), [Low, Medium, High, XHigh, Max]);
        assert_eq!(other["claude-old"].efforts(), BUDGET_EFFORTS, "a budget");
        assert_eq!(
            other["claude-old"].budget(),
            Some((Some(1_024), Option::None))
        );
        assert!(other["claude-x"].takes_effort());
        assert!(!other["claude-old"].takes_effort());

        let odd: Model = serde_json::from_str(
            r#"{"reasoning": true, "reasoning_options": [
                {"type": "effort", "values": [null, "low"]},
                {"type": "budget_tokens", "min": -1, "max": 32768}
            ]}"#,
        )
        .expect("odd values parse");
        assert_eq!(odd.efforts(), [None, Low], "null is none");
        assert_eq!(odd.budget(), Some((Option::None, Some(32_768))));
        let broken: Model =
            serde_json::from_str(r#"{"reasoning": true, "reasoning_options": [{"type": 3}]}"#)
                .expect("a broken option costs only itself");
        assert_eq!(broken.efforts(), BUDGET_EFFORTS);

        let unknown: Model =
            serde_json::from_str(r#"{"reasoning": true, "reasoning_options": [{"type": "dial"}]}"#)
                .expect("an unknown kind parses");
        assert_eq!(unknown.efforts(), []);
    }
}
