//! The interactive chat: one scrollable history above a prompt bar.

mod app;
mod chat;
mod prompt;
mod status;
mod terminal;
mod theme;

use std::{
    io::{IsTerminal, stdin, stdout},
    sync::Arc,
};

use anyhow::{Result, bail};
use nth_protocol::{Provider, Tool};
use nth_session::Session;

/// Runs the chat until the user quits. The terminal is restored on every
/// exit path, including a panic.
pub async fn run(
    session: Session,
    provider: Arc<dyn Provider>,
    tools: Arc<Vec<Box<dyn Tool>>>,
) -> Result<()> {
    // Without this check, piped or tty-less runs would write setup escape
    // codes into the pipe and then fail on raw mode.
    if !stdin().is_terminal() || !stdout().is_terminal() {
        bail!("the interactive chat needs a terminal; use `nth run` for scripted use");
    }
    let mut terminal = terminal::enter()?;
    let result = app::App::new(session, provider, tools)
        .run(&mut terminal)
        .await;
    terminal::restore();
    result
}
