//! The one built-in endpoint: a ChatGPT subscription, through the Codex
//! backend, as the Codex CLI reaches it. It speaks only responses, and
//! what it serves is the backend's own model list, which differs by plan
//! and account, at no price per token.

use nth_protocol::{Cost, Effort, ModelInfo};
use serde::Deserialize;

use super::{EndpointConfig, listing};
use crate::{
    auth::{Auth, Credentials},
    catalog::{self, Catalog},
};

pub const ID: &str = nth_auth::ChatGpt::ID;
const NAME: &str = "ChatGPT";
const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// Whose catalogue entry describes the models.
const CATALOG_ID: &str = "openai";
/// The Codex CLI version the model list is asked for. The backend leaves
/// out models whose `minimal_client_version` is above it, and nth speaks
/// the protocol of the newest Codex, so it asks as one newer than any.
const CLIENT_VERSION: &str = "999.0.0";

impl EndpointConfig {
    /// The ChatGPT endpoint, signed in through `chatgpt`. It can be built
    /// before you connect: until then its models are refused and not listed.
    pub fn chatgpt(chatgpt: nth_auth::ChatGpt) -> Self {
        Self {
            id: ID.into(),
            name: NAME.into(),
            base_url: BASE_URL.into(),
            credentials: Credentials::ChatGpt(chatgpt),
            models: Vec::new(),
        }
    }
}

/// What the catalogue knows about `model`.
pub(crate) fn known<'c>(catalog: Option<&'c Catalog>, model: &str) -> Option<&'c catalog::Model> {
    catalog?.get(CATALOG_ID)?.models.get(model)
}

#[derive(Deserialize)]
struct ModelList {
    /// Each read on its own, so one entry of a shape nth does not know
    /// costs that model, not the list.
    models: Vec<serde_json::Value>,
}

/// One model as the backend lists it; nth reads only these fields, and
/// one that leaves out whether to offer it is not offered.
#[derive(Deserialize)]
struct Listed {
    slug: String,
    display_name: Option<String>,
    /// `list` for a model to offer; `hide` for the backend's own helpers.
    #[serde(default)]
    visibility: String,
    #[serde(default)]
    supported_in_api: bool,
    context_window: Option<u64>,
    #[serde(default)]
    supported_reasoning_levels: Vec<Level>,
    #[serde(default)]
    input_modalities: Vec<String>,
}

#[derive(Deserialize)]
struct Level {
    /// A level without one is dropped, as is one nth does not know.
    #[serde(default)]
    effort: String,
}

/// The models this account is served, from the backend's list.
pub(crate) async fn list(
    http: &reqwest::Client,
    base_url: &str,
    auth: &Auth,
    catalog: Option<&Catalog>,
) -> Result<Vec<ModelInfo>, listing::Error> {
    let request = http
        .get(format!("{base_url}/models"))
        .query(&[("client_version", CLIENT_VERSION)]);
    let list: ModelList = listing::fetch(auth.bearer(request, None)).await?;
    Ok(models(list, catalog))
}

/// The listed models nth can run, with the backend's window and efforts
/// and the catalogue's output limit, which the list does not give.
fn models(list: ModelList, catalog: Option<&Catalog>) -> Vec<ModelInfo> {
    let mut models: Vec<_> = list
        .models
        .into_iter()
        .filter_map(|m| serde_json::from_value::<Listed>(m).ok())
        .filter(|m| m.visibility == "list" && m.supported_in_api)
        .map(|m| {
            let mut efforts: Vec<Effort> = m
                .supported_reasoning_levels
                .iter()
                .filter_map(|level| Effort::from_name(&level.effort))
                .collect();
            efforts.sort_by_key(|e| Effort::ALL.iter().position(|a| a == e));
            efforts.dedup();
            let output = known(catalog, &m.slug)
                .and_then(|k| k.limit.as_ref())
                .and_then(|l| l.output);
            ModelInfo {
                id: m.slug,
                name: m.display_name,
                context: m.context_window,
                output,
                efforts,
                origin: None,
                // Paid for by the subscription.
                cost: Some(Cost::default()),
                takes_images: Some(m.input_modalities.iter().any(|i| i == "image")),
            }
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(json: serde_json::Value) -> ModelList {
        serde_json::from_value(json).expect("a valid list")
    }

    fn catalog() -> Catalog {
        let catalog = serde_json::json!({
            "openai": {"npm": "@ai-sdk/openai", "models": {
                "gpt-6-luna": {"limit": {"context": 1_050_000, "output": 128_000}},
            }},
        });
        serde_json::from_value(catalog).expect("a valid catalogue")
    }

    #[test]
    fn offers_the_listed_models_at_no_price() {
        let list = list(serde_json::json!({"models": [
            {
                "slug": "gpt-6-luna",
                "display_name": "GPT-6 Luna",
                "visibility": "list",
                "supported_in_api": true,
                "context_window": 272_000,
                "input_modalities": ["text", "image"],
                "supported_reasoning_levels": [
                    {"effort": "high"}, {"effort": "low"}, {"effort": "ultra"},
                ],
                "base_instructions": "ignored",
            },
            {"slug": "gpt-5.6-luna", "visibility": "list", "supported_in_api": true},
            {"slug": "codex-auto-review", "visibility": "hide", "supported_in_api": true},
            {"slug": "gpt-chat", "visibility": "list", "supported_in_api": false},
        ]}));

        let models = models(list, Some(&catalog()));

        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-5.6-luna", "gpt-6-luna"]);
        let luna = &models[1];
        assert_eq!(luna.name.as_deref(), Some("GPT-6 Luna"));
        assert_eq!(luna.context, Some(272_000), "the subscription's window");
        assert_eq!(luna.output, Some(128_000), "the catalogue's");
        assert_eq!(
            luna.efforts,
            [Effort::Low, Effort::High],
            "least first, unknown dropped"
        );
        assert!(models.iter().all(|m| m.cost == Some(Cost::default())));
        assert_eq!(models[0].context, None, "nothing said, nothing assumed");
        assert_eq!(luna.takes_images, Some(true));
        assert_eq!(
            models[0].takes_images,
            Some(false),
            "nothing said, no images"
        );
    }

    #[test]
    fn an_entry_nth_cannot_read_costs_only_that_model() {
        let list = list(serde_json::json!({"models": [
            {"slug": "gpt-6-luna", "visibility": "list", "supported_in_api": true,
             "supported_reasoning_levels": [{"effort": "low"}, {"description": "?"}]},
            {"display_name": "No slug", "visibility": "list", "supported_in_api": true},
            {"slug": "gpt-5.6-luna", "visibility": 3, "supported_in_api": true},
            {"slug": "gpt-chat"},
            "not even an object",
        ]}));

        let models = models(list, None);

        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gpt-6-luna"], "missing what to offer is not offered");
        assert_eq!(models[0].efforts, [Effort::Low], "a level without effort");
    }

    #[test]
    fn an_empty_list_offers_nothing() {
        assert_eq!(models(list(serde_json::json!({"models": []})), None), []);
    }
}
