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
pub use chat::Show;
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

/// What the chat starts with, from the config.
pub struct Start {
    /// The model and effort each mode runs on.
    pub mode_llms: ModeLlms,
    /// Which bodies the chat shows until toggled.
    pub show: Show,
}

/// Runs the chat until the user quits, saving `session` and any other it
/// moves on to in `store` after every turn, starting as `start` says. The terminal is restored on every exit path, including a
/// panic.
pub async fn run(
    session: Session,
    provider: Arc<dyn Provider>,
    tools: Arc<Vec<Box<dyn Tool>>>,
    checkers: Checkers,
    store: Store,
    paths: Paths,
    start: Start,
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
    let result = app::App::new(session, provider, tools)
        .with_store(store)
        .with_paths(paths)
        .with_history(history)
        .with_checkers(checkers)
        .with_mode_llms(start.mode_llms)
        .with_show(start.show)
        .run(&mut terminal)
        .await;
    terminal::restore();
    result
}
