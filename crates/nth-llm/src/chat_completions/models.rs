//! Lists the models an endpoint serves. The endpoint's own `/models` (a
//! chat completions route, which every endpoint nth knows has) says what
//! exists; the catalogue says which protocol each one speaks and what its
//! limits are.

use std::time::Duration;

use nth_protocol::ModelInfo;
use serde::Deserialize;

use super::{Error, USER_AGENT, success};
use crate::catalog;

/// The list is one small JSON body, so unlike a reply it can have a deadline.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
struct ModelList {
    data: Vec<ListedModel>,
}

#[derive(Deserialize)]
struct ListedModel {
    id: String,
}

/// What the endpoint's `/models` lists.
pub(crate) async fn listed(
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

/// Only the models whose protocol nth speaks. Without a catalogue entry for
/// the endpoint, everything it lists is assumed to speak chat completions,
/// since that is what the endpoint is for. An unknown model may reason, so
/// it is offered an effort; a known one only when the catalogue says it
/// reasons.
pub(crate) fn select(
    endpoint: Vec<String>,
    provider: Option<&catalog::Provider>,
) -> Vec<ModelInfo> {
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
                    origin: None,
                });
            };
            // A model nth cannot talk to is not offered.
            provider?.wire(&id)?;
            Some(ModelInfo {
                id,
                name: model.name.clone(),
                context: model.limit.as_ref().and_then(|l| l.context),
                output: model.limit.as_ref().and_then(|l| l.output),
                reasoning: model.reasoning.unwrap_or(false),
                origin: None,
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
        [
            "kimi-k3",
            "minimax-m3",
            "grok-4.7",
            "gemini-3.5-flash",
            "glm-5.3",
            "unlisted",
        ]
        .map(String::from)
        .into()
    }

    #[test]
    fn keeps_only_models_nth_speaks_with_their_limits() {
        let catalog: catalog::Catalog = serde_json::from_str(FIXTURE).expect("valid fixture");
        let provider = catalog::provider_for(&catalog, &format!("{GO}/"));

        let models = select(endpoint(), provider);

        assert_eq!(
            ids(&models),
            ["glm-5.3", "grok-4.7", "kimi-k3", "minimax-m3", "unlisted"]
        );
        assert_eq!(
            models[0],
            ModelInfo {
                id: "glm-5.3".into(),
                name: Some("GLM-5.3".into()),
                context: Some(1_000_000),
                output: Some(131_072),
                reasoning: true,
                origin: None,
            }
        );
        assert_eq!(
            models[1].name.as_deref(),
            Some("Grok 4.7"),
            "over responses"
        );
        assert!(
            !models[2].reasoning,
            "the catalog does not say kimi reasons"
        );
        assert_eq!(
            models[3].name.as_deref(),
            Some("MiniMax M3"),
            "over messages"
        );
        assert_eq!(models[4].context, None);
        assert!(models[4].reasoning, "unlisted models may reason");
    }

    #[test]
    fn unknown_endpoint_keeps_everything() {
        let catalog: catalog::Catalog = serde_json::from_str(FIXTURE).expect("valid fixture");
        let provider = catalog::provider_for(&catalog, "http://localhost:8080/v1");

        let models = select(endpoint(), provider);

        assert_eq!(models.len(), endpoint().len());
    }
}
