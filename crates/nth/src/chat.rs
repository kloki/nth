//! `nth`: the interactive chat.

use std::sync::Arc;

use anyhow::{Context, Result};
use nth_session::Store;

use crate::{config::Config, context, post_write, setup};

/// With `resume`, the last session comes back as it was: its model, effort
/// and working directory win over the flags and where nth was started.
pub async fn run(resume: bool, config: Config) -> Result<()> {
    let paths = config.paths();
    let (mut session, provider) = setup(&config, &paths).await?;
    let store = Store::open()?;
    if resume {
        session = store
            .latest()
            .await?
            .context("no saved session to continue")?;
        let context = context(session.cwd.clone(), &paths).await;
        session.set_context(context);
        session.max_steps = config.session.max_steps;
    }
    let post_write = post_write(&config);
    // From the same servers the tools use, so the status bar shows what
    // checks the writes.
    let checkers = nth_tui::Checkers {
        lsp: post_write.lsp().clone(),
        formatters: post_write.formatters().clone(),
    };
    nth_tui::run(
        session,
        Arc::new(provider),
        Arc::new(nth_tools::all(&config.tools, post_write)),
        checkers,
        store,
        paths,
    )
    .await
}
