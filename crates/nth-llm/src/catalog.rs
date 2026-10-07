//! The models.dev catalogue: what is known about every provider's models,
//! their protocol and their limits. Shared by the protocols, which read it
//! for their own endpoints.

use std::{collections::HashMap, time::Duration};

use serde::Deserialize;

use crate::chat_completions::{Error, USER_AGENT, success};

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

#[derive(Deserialize)]
pub(crate) struct Model {
    pub(crate) name: Option<String>,
    pub(crate) reasoning: Option<bool>,
    pub(crate) limit: Option<Limit>,
    /// Set when this model needs a different SDK, and so a different wire
    /// protocol, than its provider's default.
    pub(crate) provider: Option<Override>,
}

#[derive(Deserialize)]
pub(crate) struct Limit {
    pub(crate) context: Option<u64>,
    pub(crate) output: Option<u64>,
}

#[derive(Deserialize)]
pub(crate) struct Override {
    pub(crate) npm: Option<String>,
}

pub(crate) async fn fetch(http: &reqwest::Client) -> Result<Catalog, Error> {
    let response = http
        .get(URL)
        .timeout(TIMEOUT)
        .header(reqwest::header::USER_AGENT, USER_AGENT)
        .send()
        .await?;
    Ok(success(response).await?.json().await?)
}

/// The catalogue's entry for the provider serving `base_url`, if it has one.
pub(crate) fn provider_for<'a>(catalog: &'a Catalog, base_url: &str) -> Option<&'a Provider> {
    let base_url = base_url.trim_end_matches('/');
    catalog
        .values()
        .find(|p| p.api.as_deref().map(|a| a.trim_end_matches('/')) == Some(base_url))
}
