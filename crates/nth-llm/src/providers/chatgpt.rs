//! The one built-in endpoint: a ChatGPT subscription, through the Codex
//! backend, as opencode reaches it. It has no `/models` and speaks only
//! responses; what it serves is the catalogue's OpenAI models that the
//! subscription includes, at no price per token.

use nth_protocol::{Cost, ModelInfo};

use super::EndpointConfig;
use crate::{
    auth::Credentials,
    catalog::{self, Catalog},
};

pub const ID: &str = nth_auth::ChatGpt::ID;
const NAME: &str = "ChatGPT";
const BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// Whose catalogue entry describes the models.
const CATALOG_ID: &str = "openai";

/// opencode's list of what the subscription serves, beside its rule for
/// the newer models (`offered`).
const ALLOWED: [&str; 6] = [
    "gpt-5.5",
    "gpt-5.3-codex-spark",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-6-sol",
    "gpt-6-luna",
];
const DISALLOWED: [&str; 2] = ["gpt-5.5-pro", "gpt-5.6"];

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

/// The models to offer. Without a catalogue, opencode's list with nothing
/// known about them, so an outage at models.dev does not hide ChatGPT.
pub(crate) fn models(catalog: Option<&Catalog>) -> Vec<ModelInfo> {
    let Some(openai) = catalog.and_then(|c| c.get(CATALOG_ID)) else {
        return ALLOWED
            .iter()
            .map(|id| ModelInfo {
                id: id.to_string(),
                name: None,
                context: None,
                output: None,
                efforts: catalog::BUDGET_EFFORTS.to_vec(),
                origin: None,
                cost: Some(Cost::default()),
            })
            .collect();
    };
    let mut models: Vec<_> = openai
        .models
        .iter()
        .filter(|(id, _)| offered(id))
        .map(|(id, model)| {
            let limit = model.limit.as_ref();
            // The subscription's window, not the API's.
            let (context, output) = if id.contains("gpt-5.5") || id.contains("gpt-5.6") {
                (Some(400_000), Some(128_000))
            } else {
                (limit.and_then(|l| l.context), limit.and_then(|l| l.output))
            };
            ModelInfo {
                id: id.clone(),
                name: model.name.clone(),
                context,
                output,
                efforts: model.efforts(),
                origin: None,
                // Paid for by the subscription.
                cost: Some(Cost::default()),
            }
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

/// opencode's rule: the listed ones, else a `gpt-N` or `gpt-N.M` above
/// 5.4. Its other rule, no model in pro reasoning mode, has nothing to
/// drop here: modes are catalogue variants nth does not list.
fn offered(id: &str) -> bool {
    if ALLOWED.contains(&id) {
        return true;
    }
    if DISALLOWED.contains(&id) {
        return false;
    }
    let Some(version) = id.strip_prefix("gpt-") else {
        return false;
    };
    /// The number `s` starts with, and what follows it.
    fn number(s: &str) -> (Option<u32>, &str) {
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        (s[..end].parse().ok(), &s[end..])
    }
    let (Some(major), rest) = number(version) else {
        return false;
    };
    let minor = rest
        .strip_prefix('.')
        .and_then(|rest| number(rest).0)
        .unwrap_or(0);
    (major, minor) > (5, 4)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        let ids = [
            "gpt-4.1",
            "gpt-5.2",
            "gpt-5.3-codex-spark",
            "gpt-5.4",
            "gpt-5.4-nano",
            "gpt-5.4-pro",
            "gpt-5.5",
            "gpt-5.5-pro",
            "gpt-5.6",
            "gpt-5.6-luna",
            "gpt-6-sol",
            "gpt-6.1-sol",
            "o3",
        ];
        let models: serde_json::Map<_, _> = ids
            .iter()
            .map(|id| {
                let model = serde_json::json!({
                    "name": id.to_uppercase(),
                    "reasoning": true,
                    "limit": {"context": 1_050_000, "output": 128_000},
                    "cost": {"input": 5, "output": 30},
                    "reasoning_options": [{"type": "effort", "values": ["low", "high"]}],
                });
                (id.to_string(), model)
            })
            .collect();
        let catalog = serde_json::json!({
            "openai": {"npm": "@ai-sdk/openai", "models": models},
        });
        serde_json::from_value(catalog).expect("a valid catalogue")
    }

    #[test]
    fn offers_what_the_subscription_serves_at_no_price() {
        let catalog = catalog();
        let models = models(Some(&catalog));

        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "gpt-5.3-codex-spark",
                "gpt-5.4",
                "gpt-5.5",
                "gpt-5.6-luna",
                "gpt-6-sol",
                "gpt-6.1-sol"
            ]
        );
        assert!(models.iter().all(|m| m.cost == Some(Cost::default())));
        assert_eq!(models[1].context, Some(1_050_000), "the catalogue's");
        assert_eq!(
            (models[2].context, models[2].output),
            (Some(400_000), Some(128_000)),
            "gpt-5.5 has the subscription's window"
        );
        assert_eq!(models[3].context, Some(400_000), "so has gpt-5.6-luna");
        assert_eq!(models[2].name.as_deref(), Some("GPT-5.5"));
        assert_eq!(
            models[2].efforts,
            [nth_protocol::Effort::Low, nth_protocol::Effort::High]
        );
    }

    #[test]
    fn without_a_catalogue_offers_opencodes_list() {
        let ids: Vec<_> = models(None).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ALLOWED);
    }

    #[test]
    fn newer_than_5_4_is_offered() {
        for id in ["gpt-5.5", "gpt-5.10", "gpt-6", "gpt-7-mini", "gpt-5.4-mini"] {
            assert!(offered(id), "{id}");
        }
        for id in [
            "gpt-5",
            "gpt-5.4-nano",
            "gpt-5.6",
            "gpt-5.5-pro",
            "gpt-x",
            "o4",
        ] {
            assert!(!offered(id), "{id}");
        }
    }
}
