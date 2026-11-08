//! `nth`: the interactive chat.

use std::path::PathBuf;

use anyhow::{Context, Result};
use nth_session::{Inbox, Store, Subagents};

use crate::runtime::{Runtime, Start};

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
    let (start, config) = if resume {
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
    // One for the process, like the subagents: the chat empties it whichever
    // runtime `/resume` swaps in, and every runtime's tools post to it.
    let inbox = Inbox::new();
    let prepared = runtime
        .prepare(config, providers, start, subagents.clone(), inbox.clone())
        .await?;
    let settings = settings(runtime, subagents.clone(), inbox)?;
    nth_tui::run(prepared, subagents, subagent_rx, store, settings).await
}

/// The chat's `/connect` and `/resume` on `runtime`'s ChatGPT login, the
/// one its providers sign with. What the config says about the chat
/// itself travels with each prepared runtime.
pub(crate) fn settings(
    runtime: Runtime,
    subagents: Subagents,
    inbox: Inbox,
) -> Result<nth_tui::Settings> {
    Ok(nth_tui::Settings {
        chatgpt: runtime.chatgpt()?,
        loader: Some(runtime.reload(subagents, inbox)),
    })
}
