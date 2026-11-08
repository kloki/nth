//! `nth`: the interactive chat.

use std::path::PathBuf;

use anyhow::{Context, Result};
use nth_session::{Store, Subagents};

use crate::{
    config::Config,
    runtime::{Runtime, Start},
};

/// A new chat starts in the config's default mode. With `resume`, the last
/// session comes back as it was: its mode, model, effort and working
/// directory win over the flags and where nth was started.
pub async fn start(
    resume: bool,
    config_path: Option<PathBuf>,
    model: Option<String>,
) -> Result<()> {
    let store = Store::open()?;
    let runtime = Runtime::new(config_path, model);
    let (start, mut config) = if resume {
        let saved = store
            .latest()
            .await?
            .context("no saved session to continue")?;
        let start = Start::Saved(Box::new(saved));
        let config = runtime.config_for(&start)?;
        (start, config)
    } else {
        let cwd = std::env::current_dir().context("no working directory")?;
        let config = runtime.load(&cwd)?;
        let mode = config.mode.default;
        (Start::Fresh { cwd, mode }, config)
    };
    let providers = std::sync::Arc::new(runtime.providers(&config)?);
    // The subagents the task tool starts report to the chat, a tab each,
    // whichever runtime `/resume` swaps in.
    let (subagent_tx, subagent_rx) = tokio::sync::mpsc::channel(256);
    let subagents = Subagents::new(subagent_tx);
    let prepared = runtime
        .prepare(&mut config, providers, start, subagents.clone())
        .await?;
    let settings = settings(&config, runtime, subagents.clone())?;
    nth_tui::run(prepared, subagents, subagent_rx, store, settings).await
}

/// The chat's settings from `config`, with `/connect` and `/resume` on
/// `runtime`'s ChatGPT login, the one its providers sign with.
pub(crate) fn settings(
    config: &Config,
    runtime: Runtime,
    subagents: Subagents,
) -> Result<nth_tui::Settings> {
    Ok(nth_tui::Settings {
        notifier: config.notify.notifier(),
        // The chat starts on the config's toggles; `/settings` flips them
        // for the run.
        chat: nth_tui::ChatSettings {
            thinking: config.chat.thinking,
            tool_output: config.chat.tool_output,
        },
        split_width: config.layout.split_width,
        chatgpt: runtime.chatgpt()?,
        loader: Some(runtime.reload(subagents)),
    })
}
