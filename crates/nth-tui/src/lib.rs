//! The interactive chat: one scrollable history above a prompt bar.

mod app;
mod chat;
mod command;
mod diagnostics;
mod git;
mod header;
mod history;
mod llm_picker;
mod mention;
mod monitor;
mod plan;
mod popup;
mod prompt;
mod question;
mod rich;
mod session_picker;
mod spinner;
mod status;
mod terminal;
mod theme;

use std::{
    io::{IsTerminal, stdin, stdout},
    sync::Arc,
};

use anyhow::{Result, bail};
pub use app::{Llm, ModeLlms};
use nth_context::Paths;
use nth_format::Formatters;
use nth_lsp::Lsp;
use nth_protocol::{Provider, Tool};
use nth_session::{Session, Store};

/// What checks the tools' writes: the same language servers and formatters
/// the tools use. The status bar shows the servers' states, and the
/// diagnostics tab what applies to the project.
#[derive(Clone)]
pub struct Checkers {
    pub lsp: Lsp,
    pub formatters: Arc<Formatters>,
}

/// What the chat runs commands with: the model's tools, and the shell for
/// the commands you type after `!`.
pub struct Tools {
    pub model: Arc<Vec<Box<dyn Tool>>>,
    pub shell: Arc<dyn Tool>,
}

/// Runs the chat until the user quits, saving `session` and any other it
/// moves on to in `store` after every turn. Each mode runs on its model in
/// `mode_llms`. The terminal is restored on every exit path, including a
/// panic.
pub async fn run(
    session: Session,
    provider: Arc<dyn Provider>,
    tools: Tools,
    checkers: Checkers,
    store: Store,
    paths: Paths,
    mode_llms: ModeLlms,
) -> Result<()> {
    // Without this check, piped or tty-less runs would write setup escape
    // codes into the pipe and then fail on raw mode.
    if !stdin().is_terminal() || !stdout().is_terminal() {
        bail!("the interactive chat needs a terminal; use `nth run` for scripted use");
    }
    // Without a data directory, prompts are still recalled for this run.
    let history = match history::History::path() {
        Ok(path) => history::History::load(path).await,
        Err(_) => history::History::default(),
    };
    let mut terminal = terminal::enter()?;
    let result = app::App::new(session, provider, tools.model)
        .with_shell(tools.shell)
        .with_store(store)
        .with_paths(paths)
        .with_history(history)
        .with_checkers(checkers)
        .with_mode_llms(mode_llms)
        .run(&mut terminal)
        .await;
    terminal::restore();
    result
}
