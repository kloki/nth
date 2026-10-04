//! `nth`: the interactive chat.

use std::sync::Arc;

use anyhow::{Context, Result};
use nth_protocol::{Effort, Mode};
use nth_session::Store;
use nth_tui::{Llm, ModeLlms};

use crate::{config::Config, context, post_write, setup};

/// A new chat starts in the config's default mode. With `resume`, the last
/// session comes back as it was: its mode, model, effort and working
/// directory win over the flags and where nth was started.
pub async fn run(resume: bool, config: Config) -> Result<()> {
    let paths = config.paths();
    let (mut session, provider) = setup(&config, &paths, config.mode.default).await?;
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
        mode_llms,
    )
    .await
}

fn llm((model, effort): (String, Effort)) -> Llm {
    Llm { model, effort }
}
