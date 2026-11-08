//! The providers a session talks through, and a model for it when the
//! config names none.

use anyhow::{Context, Result, bail};
use nth_context::Paths;
use nth_llm::{Credentials, EndpointConfig, Providers, Unavailable};
use nth_protocol::{Listing, Mode, Provider};

use crate::config::Config;

/// Any one of the models the providers list.
pub(super) async fn random_model(providers: &Providers) -> Result<String> {
    let listing = providers
        .models()
        .await
        .map_err(|e| anyhow::anyhow!("no model set, and listing models failed: {e}"))?;
    pick(listing)
}

/// A model from `listing` at random; none listed is an error naming the
/// providers that could not be asked.
fn pick(mut listing: Listing) -> Result<String> {
    if listing.models.is_empty() {
        let failed: Vec<_> = listing
            .failed
            .iter()
            .map(|f| format!("{}: {}", f.origin.id, f.error))
            .collect();
        bail!(
            "no model set, and the providers list none to pick from{}",
            match failed.as_slice() {
                [] => String::new(),
                failed => format!(" ({})", failed.join("; ")),
            }
        );
    }
    let picked = fastrand::usize(..listing.models.len());
    Ok(listing.models.swap_remove(picked).id)
}

/// The configured providers whose keys are set, and ChatGPT, which runs
/// once you connect, even mid-run. Neither a key set nor ChatGPT connected
/// is an error naming every variable; so is a model, the config's or a
/// mode's, on a provider whose key is not set or that is not connected.
pub(crate) fn providers(config: &Config) -> Result<Providers> {
    let chatgpt = chatgpt(&config.paths())?;
    let mut endpoints = Vec::new();
    let mut unavailable = Vec::new();
    for (id, provider) in &config.provider {
        match std::env::var(&provider.api_key_env) {
            Ok(api_key) if !api_key.is_empty() => endpoints.push(EndpointConfig {
                id: id.clone(),
                name: provider.name.clone(),
                base_url: provider.base_url.clone(),
                credentials: Credentials::Key(api_key),
                models: provider.models.clone(),
            }),
            _ => unavailable.push(Unavailable {
                id: id.clone(),
                api_key_env: provider.api_key_env.clone(),
            }),
        }
    }
    if endpoints.is_empty() && !chatgpt.connected() {
        if config.provider.is_empty() {
            bail!(
                "no provider configured: run `nth connect` to use ChatGPT, or add a \
                 [provider.<id>] to the config (`nth init` writes one with OpenCode Go \
                 and Zen ready to uncomment)"
            );
        }
        // Providers may share a variable.
        let mut variables: Vec<_> = unavailable.iter().map(|u| u.api_key_env.as_str()).collect();
        variables.sort();
        variables.dedup();
        bail!(
            "no provider has its API key set: {}, or run `nth connect`",
            variables.join(", ")
        );
    }
    // Last, so a bare model goes to a configured provider first.
    endpoints.push(EndpointConfig::chatgpt(chatgpt));
    let providers = Providers::new(endpoints, unavailable, &config.model)
        .with_context(|| format!("model {} cannot run", config.model))?;
    for mode in [Mode::Plan, Mode::Act] {
        let (model, _) = config.llm_for(mode);
        let what = match config.profiles.first() {
            Some(profile) => format!("{mode:?} in profile {}", profile.name),
            None => format!("{mode:?} mode"),
        };
        providers
            .check(&model)
            .with_context(|| format!("model {model} for {what} cannot run"))?;
    }
    Ok(providers)
}

/// The ChatGPT login, kept in nth's `auth.json`.
pub(crate) fn chatgpt(paths: &Paths) -> Result<nth_auth::ChatGpt> {
    let dir = paths
        .nth_data()
        .context("neither XDG_DATA_HOME nor HOME is set, so nth has nowhere to keep credentials")?;
    Ok(nth_auth::ChatGpt::new(nth_auth::Store::new(&dir))?)
}

#[cfg(test)]
mod tests {
    use nth_protocol::{Failed, ModelInfo, Origin};

    use super::*;

    fn model(id: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: None,
            context: None,
            output: None,
            efforts: Vec::new(),
            origin: None,
            cost: None,
            takes_images: None,
        }
    }

    #[test]
    fn picks_one_of_the_listed_models() {
        let ids = ["zen/a", "zen/b", "go/c"];
        let listing = Listing::from(ids.map(model).to_vec());
        for _ in 0..20 {
            let picked = pick(listing.clone()).expect("one is listed");
            assert!(ids.contains(&picked.as_str()), "{picked}");
        }
    }

    #[test]
    fn nothing_listed_names_the_providers_that_failed() {
        assert!(pick(Listing::default()).is_err());
        let listing = Listing {
            models: Vec::new(),
            failed: vec![Failed {
                origin: Origin {
                    id: "zen".into(),
                    name: "Zen".into(),
                },
                error: "401".into(),
            }],
        };
        let error = pick(listing).expect_err("nothing to pick").to_string();
        assert!(error.contains("zen: 401"), "{error}");
    }
}
