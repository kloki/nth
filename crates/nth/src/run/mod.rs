//! `nth run`: one prompt, worked on until the agent answers, for scripts
//! and pipes. The answer goes to stdout, everything else to stderr.

mod render;

use std::{sync::Arc, time::Instant};

use anyhow::{Context, Result, anyhow};
use nth_protocol::{FrontEnd, Mode, Provider};
use nth_session::{CancellationToken, Store, Subagents};
use owo_colors::OwoColorize;
use tokio::sync::mpsc;

use crate::{config::Config, post_write, setup};

pub async fn run(prompt: String, mode: Mode, config: Config) -> Result<()> {
    let (mut session, provider) = setup(&config, &config.paths(), mode).await?;
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let cwd = session.cwd.clone();
    let post_write = post_write(&config);
    let lsp = post_write.lsp().clone();
    // Without a front-end, a subagent's answer has nothing to wake the
    // model, so the task tool waits for it.
    let tools = crate::tools(&config, post_write, provider.clone(), Subagents::default());
    // `/name args` runs a skill, as in the chat.
    let prompt = match nth_context::skills::parse(&prompt, &session.context().skills) {
        Some((skill, args)) => skill
            .invoke(args, &cwd)
            .await
            .map_err(|e| anyhow!("could not run the skill: {e}"))?,
        None => prompt,
    };

    let started = Instant::now();
    let (tx, mut rx) = mpsc::channel(256);
    let printer = tokio::spawn(async move {
        let mut out = render::Printer::new(cwd);
        while let Some(event) = rx.recv().await {
            out.event(&event);
        }
        out
    });
    let turn = session
        // Nobody is there to answer or to show a panel to, so the question
        // tool tells the model to decide for itself.
        .prompt(
            prompt,
            provider.as_ref(),
            &tools,
            &FrontEnd::default(),
            &tx,
            &CancellationToken::new(),
        )
        .await;
    drop(tx);
    let printer = printer.await.context("printer task failed")?;
    printer.finish();
    // Saved even when the turn failed, so `nth -c` can pick it up. Not
    // saving is worth a warning, not a failed run.
    let saved = match Store::open() {
        Ok(store) => store.save(&session).await,
        Err(e) => Err(e),
    };
    if let Err(e) = saved {
        eprintln!("{} session not saved: {e}", "!".yellow().bold());
    }
    lsp.shutdown().await;
    turn?;

    eprintln!(
        "{} {}",
        "✓".green().bold(),
        format!(
            "{} · {} tool calls · {:.1}s",
            session.model,
            printer.tool_calls,
            started.elapsed().as_secs_f64()
        )
        .dimmed()
    );
    Ok(())
}
