//! `nth`: the interactive chat.

use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};
use nth_llm::providers::Error;
use nth_protocol::{Effort, Mode};
use nth_session::{Store, Subagents};
use nth_tui::{Llm, ModeLlms, Profile};

use crate::{config::Config, context, post_write, setup};

/// A new chat starts in the config's default mode. With `resume`, the last
/// session comes back as it was: its mode, model, effort and working
/// directory win over the flags and where nth was started.
pub async fn start(
    resume: bool,
    config_path: Option<PathBuf>,
    model: Option<String>,
) -> Result<()> {
    let store = Store::open()?;
    let saved = if resume {
        Some(
            store
                .latest()
                .await?
                .context("no saved session to continue")?,
        )
    } else {
        None
    };
    let cwd = match &saved {
        Some(session) => session.cwd.clone(),
        None => std::env::current_dir().context("no working directory")?,
    };
    let config = crate::load_for(config_path.as_deref(), model.clone(), &cwd)?;
    run(saved, store, config, config_path, model).await
}

async fn run(
    saved: Option<nth_session::Session>,
    store: Store,
    mut config: Config,
    config_path: Option<PathBuf>,
    model: Option<String>,
) -> Result<()> {
    let paths = config.paths();
    let mode = config.mode.default;
    let (mut session, provider) = if let Some(mut session) = saved {
        let provider = crate::providers(&config)?;
        let context = context(session.cwd.clone(), &paths).await;
        session.set_context(context);
        for warning in crate::apply_directories(&mut session, &config) {
            eprintln!("{warning}");
        }
        session.max_steps = config.session.max_steps;
        // Saved before there were several providers, perhaps: shown and
        // compared as provider/model like the listed ones.
        let model = provider.qualify(&session.model);
        if model != session.model {
            session.set_model(model);
        }
        (session, provider)
    } else {
        setup(&mut config, &paths, mode).await?
    };
    // The configured compaction policy; the chat takes the limits from its
    // own listing when a turn starts, so opening it waits on no endpoint.
    // A resumed session keeps the limits it saved until then.
    session.compaction = config.context;
    let (mut mode_llms, profiles) = model_settings(&config, &provider);
    // A resumed session keeps its model; the other mode starts on the
    // config's. One running as an agent keeps it for the agent, which the
    // chat takes from the session.
    let current = Llm {
        model: session.model.clone(),
        effort: session.effort,
    };
    match (session.primary(), session.mode) {
        (Some(_), _) => {}
        (None, Mode::Plan) => mode_llms.plan = current,
        (None, Mode::Act) => mode_llms.act = current,
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
    let chatgpt = crate::chatgpt(&paths)?;
    let provider = Arc::new(provider);
    // The subagents the task tool starts report to the chat, a tab each.
    let (subagent_tx, subagent_rx) = tokio::sync::mpsc::channel(256);
    let subagents = Subagents::new(subagent_tx);
    let loader = Arc::new(Loader {
        config_path,
        model,
        subagents: subagents.clone(),
    });
    let result = nth_tui::run_with_loader(
        session,
        provider.clone(),
        nth_tui::Tools {
            model: Arc::new(crate::tools(
                &config,
                post_write,
                provider.clone(),
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
            chatgpt,
        },
        Some(loader),
    )
    .await;
    // After the terminal is back: the servers get a moment to leave on
    // their own before the process ends and kills them.
    lsp.shutdown().await;
    result
}

struct Loader {
    config_path: Option<PathBuf>,
    model: Option<String>,
    subagents: Subagents,
}

impl nth_tui::RuntimeLoader for Loader {
    fn prepare(
        &self,
        mut session: nth_session::Session,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<nth_tui::PreparedRuntime>> + Send + '_>,
    > {
        Box::pin(async move {
            let config = crate::load_for(
                self.config_path.as_deref(),
                self.model.clone(),
                &session.cwd,
            )?;
            let provider = Arc::new(crate::providers(&config)?);
            let paths = config.paths();
            session.set_context(context(session.cwd.clone(), &paths).await);
            let warnings = crate::apply_directories(&mut session, &config);
            session.max_steps = config.session.max_steps;
            session.compaction = config.context;
            let (mode_llms, profiles) = model_settings(&config, &provider);
            let post_write = post_write(&config);
            let checkers = nth_tui::Checkers {
                lsp: post_write.lsp().clone(),
                formatters: post_write.formatters().clone(),
                rtk: config.tools.bash.rtk,
            };
            Ok(nth_tui::PreparedRuntime {
                session,
                paths,
                checkers,
                mode_llms,
                profiles,
                provider: provider.clone(),
                model_tools: Arc::new(crate::tools(
                    &config,
                    post_write,
                    provider,
                    self.subagents.clone(),
                )),
                shell: Arc::new(nth_tools::Bash::untimed(config.tools.bash.clone())),
                warnings,
            })
        })
    }
}

fn model_settings(config: &Config, provider: &nth_llm::Providers) -> (ModeLlms, Vec<Profile>) {
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
