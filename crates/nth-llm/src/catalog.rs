//! The models.dev catalogue: what is known about every provider's models,
//! their protocol and their limits. Fetched once and shared: the listing
//! reads names and limits from it, and a request the protocol to speak.

use std::{collections::HashMap, sync::Arc, time::Duration};

use nth_protocol::{Cost, Effort};
use serde::Deserialize;
use tokio::sync::OnceCell;

use crate::http::USER_AGENT;

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

/// The catalogue, fetched by whichever needs it first and then kept for
/// the life of the process. A failed fetch is kept too, as no catalogue:
/// every model then speaks chat completions, as with an unknown endpoint.
#[derive(Clone)]
pub(crate) struct Snapshot {
    http: reqwest::Client,
    url: String,
    catalog: Arc<OnceCell<Option<Catalog>>>,
}

impl Snapshot {
    pub(crate) fn new(http: reqwest::Client) -> Self {
        Self {
            http,
            url: URL.into(),
            catalog: Arc::new(OnceCell::new()),
        }
    }

    /// The catalogue, waiting for the fetch if it is still under way.
    pub(crate) async fn get(&self) -> Option<&Catalog> {
        self.catalog
            .get_or_init(|| async { fetch(&self.http, &self.url).await.ok() })
            .await
            .as_ref()
    }

    #[cfg(test)]
    pub(crate) fn of(catalog: Catalog) -> Self {
        Self {
            catalog: Arc::new(OnceCell::new_with(Some(Some(catalog)))),
            ..Self::new(reqwest::Client::new())
        }
    }

    /// One fetched from `url` instead of models.dev.
    #[cfg(test)]
    pub(crate) fn at(url: String) -> Self {
        Self {
            url,
            ..Self::new(reqwest::Client::new())
        }
    }
}

async fn fetch(http: &reqwest::Client, url: &str) -> Result<Catalog, reqwest::Error> {
    http.get(url)
        .timeout(TIMEOUT)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
}

/// The catalogue's entry for the provider serving `base_url`, else the one
/// named `id`: some entries, as OpenAI's, give no `api` to match. With the
/// id the catalogue knows it by.
pub(crate) fn provider_for<'a>(
    catalog: &'a Catalog,
    id: &str,
    base_url: &str,
) -> Option<(&'a str, &'a Provider)> {
    let base_url = base_url.trim_end_matches('/');
    catalog
        .iter()
        .find(|(_, p)| p.api.as_deref().map(|a| a.trim_end_matches('/')) == Some(base_url))
        .or_else(|| catalog.get_key_value(id))
        .map(|(id, p)| (id.as_str(), p))
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

    const FIXTURE: &str = include_str!("../tests/fixtures/models_dev.json");

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
