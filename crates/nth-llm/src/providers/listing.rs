//! Lists the models the endpoints serve. An endpoint's own `/models` (an
//! OpenAI-shaped route, which every endpoint nth knows has, whatever its
//! models speak) says what exists; the catalogue says which protocol each
//! one speaks and what its limits are. Every endpoint's list then goes
//! into the one listing, each id prefixed with its endpoint's.

use std::time::Duration;

use nth_protocol::{Failed, Listing, ModelInfo, Origin};
use serde::{Deserialize, de::DeserializeOwned};

use crate::{auth::Auth, catalog};

/// The list is one small JSON body, so unlike a reply it can have a deadline.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Auth(#[from] nth_auth::Error),
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
    auth: &Auth,
) -> Result<Vec<String>, Error> {
    let list: ModelList = fetch(auth.bearer(http.get(format!("{base_url}/models")), None)).await?;
    Ok(list.data.into_iter().map(|m| m.id).collect())
}

/// The JSON body `request` answers with, under the list's deadline; a
/// non-2xx answer is an error carrying its body.
pub(crate) async fn fetch<T: DeserializeOwned>(
    request: reqwest::RequestBuilder,
) -> Result<T, Error> {
    let response = request.timeout(LIST_TIMEOUT).send().await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(Error::Status { status, body });
    }
    Ok(response.json().await?)
}

/// Only the models whose protocol nth speaks. Without a catalogue entry for
/// the endpoint, everything it lists is assumed to speak chat completions,
/// since that is what the endpoint is for, and what the catalogue knows of
/// a model under another provider is borrowed, all but its price. An
/// unknown model may reason, so it is offered the usual efforts; a known
/// one those the catalogue says it takes.
pub(crate) fn select(
    endpoint: Vec<String>,
    catalog: Option<&catalog::Catalog>,
    provider: Option<&catalog::Provider>,
) -> Vec<ModelInfo> {
    let mut models: Vec<_> = endpoint
        .into_iter()
        .filter_map(|id| {
            let (model, cost) = match provider {
                Some(provider) => {
                    // A model nth cannot talk to is not offered.
                    provider.wire(&id)?;
                    let model = provider.models.get(&id);
                    (model, model.and_then(|m| m.cost))
                }
                // Another provider's price is not this endpoint's.
                None => (catalog.and_then(|c| catalog::borrowed(c, &id)), None),
            };
            let Some(model) = model else {
                return Some(ModelInfo {
                    id,
                    name: None,
                    context: None,
                    output: None,
                    efforts: catalog::BUDGET_EFFORTS.to_vec(),
                    origin: None,
                    cost: None,
                    takes_images: None,
                });
            };
            Some(ModelInfo {
                id,
                name: model.name.clone(),
                context: model.limit.as_ref().and_then(|l| l.context),
                output: model.limit.as_ref().and_then(|l| l.output),
                efforts: model.efforts(),
                origin: None,
                cost,
                takes_images: Some(model.takes_images()),
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
            takes_images: Some(false),
        }
    }

    #[test]
    fn keeps_only_models_nth_speaks_with_their_limits() {
        let catalog: catalog::Catalog = serde_json::from_str(FIXTURE).expect("valid fixture");
        let provider =
            catalog::provider_for(&catalog, "opencode", &format!("{GO}/")).map(|(_, p)| p);

        let models = select(endpoint(), Some(&catalog), provider);

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
                takes_images: Some(false),
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
        assert_eq!(models[4].takes_images, None, "nor is it known to see");
    }

    #[test]
    fn zen_lists_grok_metadata_by_url_under_a_configured_alias() {
        let catalog: catalog::Catalog = serde_json::from_str(FIXTURE).expect("valid fixture");
        let (id, provider) =
            catalog::provider_for(&catalog, "zen", "https://opencode.ai/inference/openai/v1/")
                .expect("Zen matched by URL");
        assert_eq!(id, "opencode");
        let models = select(
            ["grok-code", "grok-4.7", "grok-build-0.1"]
                .map(String::from)
                .into(),
            Some(&catalog),
            Some(provider),
        );

        assert_eq!(ids(&models), ["grok-4.7", "grok-build-0.1", "grok-code"]);
        assert_eq!(
            models[0],
            ModelInfo {
                id: "grok-4.7".into(),
                name: Some("Grok 4.7".into()),
                context: Some(500_000),
                output: Some(500_000),
                efforts: vec![Effort::Low, Effort::Medium, Effort::High, Effort::XHigh],
                origin: None,
                cost: Some(Cost {
                    input: 2.0,
                    output: 6.0,
                    cache_read: Some(0.5),
                    cache_write: None,
                }),
                takes_images: Some(true),
            }
        );
        assert_eq!(models[1].name.as_deref(), Some("Grok Build 0.1"));
        assert_eq!(models[1].takes_images, Some(true));
        assert!(models[1].efforts.is_empty());
        assert_eq!(models[2].name.as_deref(), Some("Grok Code"));
        assert_eq!(models[2].takes_images, Some(false));
        assert!(models[2].efforts.is_empty());
    }

    #[test]
    fn unknown_endpoint_keeps_everything() {
        let catalog: catalog::Catalog = serde_json::from_str(FIXTURE).expect("valid fixture");
        let provider =
            catalog::provider_for(&catalog, "local", "http://localhost:8080/v1").map(|(_, p)| p);

        let models = select(endpoint(), Some(&catalog), provider);

        assert_eq!(models.len(), endpoint().len());
    }

    #[test]
    fn an_unknown_endpoint_borrows_all_but_the_price() {
        let catalog: catalog::Catalog = serde_json::from_str(FIXTURE).expect("valid fixture");
        let lyceum = ["z-ai/glm-5.3", "acme/grok-4.7", "unlisted"].map(String::from);

        let models = select(lyceum.into(), Some(&catalog), None);

        assert_eq!(ids(&models), ["acme/grok-4.7", "unlisted", "z-ai/glm-5.3"]);
        assert_eq!(
            models[0].efforts,
            [Effort::Low, Effort::Medium, Effort::High, Effort::XHigh],
            "by the bare name"
        );
        assert_eq!(models[0].cost, None, "go's price is go's");
        assert_eq!(models[1].efforts, catalog::BUDGET_EFFORTS);
        assert_eq!(
            models[2],
            ModelInfo {
                id: "z-ai/glm-5.3".into(),
                name: Some("GLM-5.3".into()),
                context: Some(204_800),
                output: Some(131_072),
                efforts: Vec::new(),
                origin: None,
                cost: None,
                takes_images: Some(false),
            },
            "the canonical entry, zhipuai's"
        );
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
