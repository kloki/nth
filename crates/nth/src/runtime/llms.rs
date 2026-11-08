//! What each mode, the subagents and the clerk run on, and the profiles
//! ctrl+p cycles.

use nth_config::Config;
use nth_llm::{Providers, providers::Error};
use nth_protocol::{Effort, Mode};
use nth_tui::{Llm, ModeLlms, Profile};

pub(super) fn model_settings(config: &Config, provider: &Providers) -> (ModeLlms, Vec<Profile>) {
    let llm = |(model, effort): (String, Effort)| Llm {
        model: provider.qualify(&model),
        effort,
    };
    // One per `[[profiles]]`, with why it cannot run when its provider
    // has no key set: the chat skips those when cycling. Whether ChatGPT
    // is connected is left to the chat, which asks on every cycle, since
    // `/connect` changes it mid-run. The first is what the chat starts
    // on; with none, `mode_llms` are the config's.
    let profiles: Vec<Profile> = config
        .profiles
        .iter()
        .map(|profile| {
            let llms = ModeLlms {
                plan: llm(config.profile_llm(profile, Mode::Plan)),
                act: llm(config.profile_llm(profile, Mode::Act)),
                subagent: config.profile_subagent(profile).map(llm),
                clerk: config.profile_clerk(profile).map(llm),
            };
            let runs: Vec<&Llm> = [&llms.plan, &llms.act]
                .into_iter()
                .chain(&llms.subagent)
                .chain(&llms.clerk)
                .collect();
            let unavailable = runs
                .iter()
                .find_map(|llm| match provider.check(&llm.model) {
                    Err(Error::Auth(nth_auth::Error::NotConnected)) | Ok(()) => None,
                    Err(error) => Some(why_unavailable(error)),
                });
            let on_chatgpt = |llm: &&Llm| {
                llm.model
                    .split_once('/')
                    .is_some_and(|(id, _)| id == nth_auth::ChatGpt::ID)
            };
            Profile {
                name: profile.name.clone(),
                needs_chatgpt: runs.iter().any(on_chatgpt),
                llms,
                unavailable,
            }
        })
        .collect();
    let mode_llms = ModeLlms {
        plan: llm(config.llm_for(Mode::Plan)),
        act: llm(config.llm_for(Mode::Act)),
        subagent: config.subagent_for().map(llm),
        clerk: config.clerk_for().map(llm),
    };
    (mode_llms, profiles)
}

/// Why a profile cannot run, short, for the status bar hint that says it
/// was skipped when cycling.
fn why_unavailable(error: Error) -> String {
    match error {
        Error::Unavailable { api_key_env, .. } => format!("{api_key_env} is not set"),
        error => error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unavailable_profile_says_what_is_missing() {
        let error = Error::Unavailable {
            id: "zen".into(),
            api_key_env: "ZEN_API_KEY".into(),
        };
        assert_eq!(why_unavailable(error), "ZEN_API_KEY is not set");
    }
}
