//! `nth`: the interactive chat.

use std::sync::Arc;

use anyhow::{Context, Result};
use nth_llm::providers::Error;
use nth_protocol::{Effort, Mode, Provider};
use nth_session::{Store, Subagents};
use nth_tui::{Llm, ModeLlms, Profile};

use crate::{config::Config, context, post_write, setup};

/// A new chat starts in the config's default mode. With `resume`, the last
/// session comes back as it was: its mode, model, effort and working
/// directory win over the flags and where nth was started.
pub async fn run(resume: bool, mut config: Config) -> Result<()> {
    let paths = config.paths();
    let mode = config.mode.default;
    let (mut session, provider) = setup(&mut config, &paths, mode).await?;
    let store = Store::open()?;
    if resume {
        session = store
            .latest()
            .await?
            .context("no saved session to continue")?;
        let context = context(session.cwd.clone(), &paths).await;
        session.set_context(context);
        session.max_steps = config.session.max_steps;
        // Saved before there were several providers, perhaps: shown and
        // compared as provider/model like the listed ones.
        let model = provider.qualify(&session.model);
        if model != session.model {
            session.set_model(model);
        }
    }
    // The configured compaction policy; the chat takes the limits from its
    // own listing when a turn starts, so opening it waits on no endpoint.
    // A resumed session keeps the limits it saved until then.
    session.compaction = config.context;
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
            };
            let runs = [&llms.plan, &llms.act];
            let unavailable = runs
                .into_iter()
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
    let mut mode_llms = ModeLlms {
        plan: llm(config.llm_for(Mode::Plan)),
        act: llm(config.llm_for(Mode::Act)),
    };
    // A resumed session keeps its model; the other mode starts on the
    // config's.
    let current = Llm {
        model: session.model.clone(),
        effort: session.effort,
    };
    match session.mode {
        Mode::Plan => mode_llms.plan = current,
        Mode::Act => mode_llms.act = current,
    }
    let post_write = post_write(&config);
    // From the same servers the tools use, so the status bar shows what
    // checks the writes.
    let lsp = post_write.lsp().clone();
    let checkers = nth_tui::Checkers {
        lsp: lsp.clone(),
        formatters: post_write.formatters().clone(),
        rtk: config.tools.bash.rtk,
    };
    let provider: Arc<dyn Provider> = Arc::new(provider);
    // The subagents the task tool starts report to the chat, a tab each.
    let (subagent_tx, subagent_rx) = tokio::sync::mpsc::channel(256);
    let subagents = Subagents::new(subagent_tx);
    let result = nth_tui::run(
        session,
        provider.clone(),
        nth_tui::Tools {
            model: Arc::new(crate::tools(
                &config,
                post_write,
                provider,
                subagents.clone(),
            )),
            // You watch what you run and stop it yourself.
            shell: Arc::new(nth_tools::Bash::untimed(config.tools.bash.clone())),
            subagents,
            subagent_rx,
        },
        checkers,
        store,
        nth_tui::Settings {
            paths,
            mode_llms,
            profiles,
            notifier: config.notify.notifier(),
            // The chat starts on the config's toggles; `/settings` flips
            // them for the run.
            chat: nth_tui::ChatSettings {
                thinking: config.chat.thinking,
                tool_output: config.chat.tool_output,
            },
            split_width: config.layout.split_width,
            chatgpt: crate::chatgpt()?,
        },
    )
    .await;
    // After the terminal is back: the servers get a moment to leave on
    // their own before the process ends and kills them.
    lsp.shutdown().await;
    result
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
