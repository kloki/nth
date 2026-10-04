//! The interactive chat: one scrollable history above a prompt bar.

mod app;
mod chat;
mod command;
mod git;
mod header;
mod history;
mod llm_picker;
mod mention;
mod popup;
mod prompt;
mod question;
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
use nth_context::Paths;
use nth_protocol::{Provider, Tool};
use nth_session::{Session, Store};

/// Runs the chat until the user quits, saving `session` and any other it
/// moves on to in `store` after every turn. The terminal is restored on
/// every exit path, including a panic.
pub async fn run(
    session: Session,
    provider: Arc<dyn Provider>,
    tools: Arc<Vec<Box<dyn Tool>>>,
    store: Store,
    paths: Paths,
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
        .run(&mut terminal)
        .await;
    terminal::restore();
    result
}
