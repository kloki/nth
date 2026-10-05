//! Lists the models an endpoint serves over chat completions. The endpoint's
//! own `/models` says what exists; models.dev says which protocol each one
//! speaks and what its limits are.

use std::{collections::HashMap, time::Duration};

use nth_protocol::ModelInfo;
use serde::Deserialize;

use super::{Error, USER_AGENT, success};

const CATALOG_URL: &str = "https://models.dev/api.json";
/// models.dev only refines the list, so a stall must not hold it up.
const CATALOG_TIMEOUT: Duration = Duration::from_secs(5);
/// The list is one small JSON body, so unlike a reply it can have a deadline.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
const CHAT_COMPLETIONS_NPM: &str = "@ai-sdk/openai-compatible";

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ListedModel>,
}

#[derive(Deserialize)]
struct ListedModel {
    id: String,
}

#[derive(Deserialize)]
struct CatalogProvider {
    npm: Option<String>,
    api: Option<String>,
    #[serde(default)]
    models: HashMap<String, CatalogModel>,
}

#[derive(Deserialize)]
struct CatalogModel {
    name: Option<String>,
    reasoning: Option<bool>,
    limit: Option<Limit>,
    /// Set when this model needs a different SDK, and so a different wire
    /// protocol, than its provider's default.
    provider: Option<Override>,
}

#[derive(Deserialize)]
struct Limit {
    context: Option<u64>,
    output: Option<u64>,
}

#[derive(Deserialize)]
struct Override {
    npm: Option<String>,
}

pub async fn list(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>, Error> {
    let (endpoint, catalog) = futures::join!(listed(http, base_url, api_key), catalog(http));
    let endpoint = endpoint?;
    let catalog = catalog.ok();
    let provider = catalog.as_ref().and_then(|c| provider_for(c, base_url));
    Ok(select(endpoint, provider))
}

async fn listed(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<String>, Error> {
    let response = http
        .get(format!("{base_url}/models"))
        .timeout(LIST_TIMEOUT)
        .bearer_auth(api_key)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await?;
    let list: ModelList = success(response).await?.json().await?;
    Ok(list.data.into_iter().map(|m| m.id).collect())
}

async fn catalog(http: &reqwest::Client) -> Result<HashMap<String, CatalogProvider>, Error> {
    let response = http
        .get(CATALOG_URL)
        .timeout(CATALOG_TIMEOUT)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await?;
    Ok(success(response).await?.json().await?)
}

fn provider_for<'a>(
    catalog: &'a HashMap<String, CatalogProvider>,
    base_url: &str,
) -> Option<&'a CatalogProvider> {
    let base_url = base_url.trim_end_matches('/');
    catalog
        .values()
        .find(|p| p.api.as_deref().map(|a| a.trim_end_matches('/')) == Some(base_url))
}

/// Without a catalog entry for the endpoint, everything it lists is assumed
/// to speak chat completions, since that is what the endpoint is for. An
/// unknown model may reason, so it is offered an effort; a known one only
/// when the catalog says it reasons.
fn select(endpoint: Vec<String>, provider: Option<&CatalogProvider>) -> Vec<ModelInfo> {
    let mut models: Vec<_> = endpoint
        .into_iter()
        .filter_map(|id| {
            let Some(model) = provider.and_then(|p| p.models.get(&id)) else {
                return Some(ModelInfo {
                    id,
                    name: None,
                    context: None,
                    output: None,
                    reasoning: true,
                });
            };
            let npm = model
                .provider
                .as_ref()
                .and_then(|o| o.npm.as_deref())
                .or(provider.and_then(|p| p.npm.as_deref()));
            if npm.is_some_and(|npm| npm != CHAT_COMPLETIONS_NPM) {
                return None;
            }
            Some(ModelInfo {
                id,
                name: model.name.clone(),
                context: model.limit.as_ref().and_then(|l| l.context),
                output: model.limit.as_ref().and_then(|l| l.output),
                reasoning: model.reasoning.unwrap_or(false),
            })
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/models_dev.json");
    const GO: &str = "https://opencode.ai/zen/go/v1";

    fn ids(models: &[ModelInfo]) -> Vec<&str> {
        models.iter().map(|m| m.id.as_str()).collect()
    }

    fn endpoint() -> Vec<String> {
        ["kimi-k3", "minimax-m3", "grok-4.7", "glm-5.3", "unlisted"]
            .map(String::from)
            .into()
    }

    #[test]
    fn keeps_only_chat_completions_models_with_their_limits() {
        let catalog: HashMap<String, CatalogProvider> =
            serde_json::from_str(FIXTURE).expect("valid fixture");
        let provider = provider_for(&catalog, &format!("{GO}/"));

        let models = select(endpoint(), provider);

        assert_eq!(ids(&models), ["glm-5.3", "kimi-k3", "unlisted"]);
        assert_eq!(
            models[0],
            ModelInfo {
                id: "glm-5.3".into(),
                name: Some("GLM-5.3".into()),
                context: Some(1_000_000),
                output: Some(131_072),
                reasoning: true,
            }
        );
        assert!(
            !models[1].reasoning,
            "the catalog does not say kimi reasons"
        );
        assert_eq!(models[2].context, None);
        assert!(models[2].reasoning, "unlisted models may reason");
    }

    #[test]
    fn unknown_endpoint_keeps_everything() {
        let catalog: HashMap<String, CatalogProvider> =
            serde_json::from_str(FIXTURE).expect("valid fixture");
        let provider = provider_for(&catalog, "http://localhost:8080/v1");

        let models = select(endpoint(), provider);

        assert_eq!(models.len(), endpoint().len());
    }
}
