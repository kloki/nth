//! Lists the models the endpoints serve. An endpoint's own `/models` (an
//! OpenAI-shaped route, which every endpoint nth knows has, whatever its
//! models speak) says what exists; the catalogue says which protocol each
//! one speaks and what its limits are. Every endpoint's list then goes
//! into the one listing, each id prefixed with its endpoint's.

use std::time::Duration;

use nth_protocol::{Failed, Listing, ModelInfo, Origin};
use serde::Deserialize;

use crate::{catalog, http::USER_AGENT};

/// The list is one small JSON body, so unlike a reply it can have a deadline.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{status}: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
    },
}

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
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(Error::Status { status, body });
    }
    let list: ModelList = response.json().await?;
    Ok(list.data.into_iter().map(|m| m.id).collect())
}

/// Only the models whose protocol nth speaks. Without a catalogue entry for
/// the endpoint, everything it lists is assumed to speak chat completions,
/// since that is what the endpoint is for. An unknown model may reason, so
/// it is offered the usual efforts; a known one those the catalogue says it
/// takes.
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
                    efforts: catalog::BUDGET_EFFORTS.to_vec(),
                    origin: None,
                    cost: None,
                });
            };
            // A model nth cannot talk to is not offered.
            provider?.wire(&id)?;
            Some(ModelInfo {
                id,
                name: model.name.clone(),
                context: model.limit.as_ref().and_then(|l| l.context),
                output: model.limit.as_ref().and_then(|l| l.output),
                efforts: model.efforts(),
                origin: None,
                cost: model.cost,
            })
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

/// One listing from every endpoint's: ids prefixed with their endpoint,
/// and an endpoint that could not list reported beside them. Only when
/// none could is the listing itself a failure.
pub(crate) fn merge<E: std::fmt::Display>(
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
    use nth_protocol::{Cost, Effort};

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
            efforts: Vec::new(),
            origin: None,
            cost: None,
        }
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
                efforts: vec![Effort::Low, Effort::High, Effort::Max],
                origin: None,
                cost: Some(Cost {
                    input: 1.4,
                    output: 4.4,
                    cache_read: Some(0.26),
                    cache_write: None,
                }),
            }
        );
        assert_eq!(
            models[1].name.as_deref(),
            Some("Grok 4.7"),
            "over responses"
        );
        assert_eq!(models[2].efforts, [Effort::Max], "kimi only maxes");
        assert_eq!(
            models[3].name.as_deref(),
            Some("MiniMax M3"),
            "over messages"
        );
        assert_eq!(models[4].context, None);
        assert_eq!(models[3].efforts, [], "minimax only toggles");
        assert_eq!(
            models[4].efforts,
            catalog::BUDGET_EFFORTS,
            "unlisted models may reason"
        );
    }

    #[test]
    fn unknown_endpoint_keeps_everything() {
        let catalog: catalog::Catalog = serde_json::from_str(FIXTURE).expect("valid fixture");
        let provider = catalog::provider_for(&catalog, "http://localhost:8080/v1");

        let models = select(endpoint(), provider);

        assert_eq!(models.len(), endpoint().len());
    }

    #[test]
    fn merging_prefixes_ids_and_reports_the_endpoints_that_failed() {
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
    fn merging_fails_only_when_every_endpoint_did() {
        let error = merge::<&str>(vec![
            (origin("lyceum"), Err("offline")),
            (origin("opencode"), Err("401")),
        ])
        .expect_err("nothing listed");
        assert_eq!(error, "offline");
        assert_eq!(merge::<&str>(Vec::new()), Ok(Listing::default()));
    }
}
