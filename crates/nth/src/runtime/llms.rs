//! What each mode, the subagents and the clerk run on, and the profiles
//! ctrl+p cycles.

use nth_config::{Config, Selection};
use nth_llm::{Providers, providers::Error};

/// What `config` starts a chat on, its ids named as `provider` lists them,
/// and each profile whose provider has no key set marked so cycling skips
/// it. A ChatGPT login missing is not such a mark: the chat asks on every
/// cycle, since `/connect` changes it mid-run.
pub(super) fn selection(config: &Config, provider: &Providers) -> Selection {
    Selection::from_config(
        config,
        &|model| provider.qualify(model),
        &|model| match provider.check(model) {
            Err(Error::Auth(nth_auth::Error::NotConnected)) | Ok(()) => None,
            Err(error) => Some(why_unavailable(error)),
        },
    )
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
