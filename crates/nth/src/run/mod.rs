//! `nth run`: one prompt, worked on until the agent answers, for scripts
//! and pipes. The answer goes to stdout, everything else to stderr.

mod render;

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use nth_icons::icons;
use nth_protocol::{FrontEnd, Mode, Provider, Tool};
use nth_session::{
    CancellationToken, Price, Session, Spend, Store, Subagents, Total, store,
    usage::{self, short, steps},
};
use owo_colors::OwoColorize;
use tokio::sync::mpsc;

use crate::{apply_context, config::Config, post_write, setup};

/// How long an interrupted run waits for its subagents' turns to end, so
/// what they spent is counted.
const SUBAGENTS_WAIT: Duration = Duration::from_secs(2);
/// How often that wait looks.
const SUBAGENT_POLL: Duration = Duration::from_millis(20);

pub async fn run(
    prompt: String,
    mode: Mode,
    agent: Option<String>,
    mut config: Config,
) -> Result<()> {
    let paths = config.paths();
    let (mut session, provider) = setup(&mut config, &paths, mode).await?;
    if let Some(name) = agent {
        let agents = &session.context().agents;
        let Some(agent) = agents.primary(&name) else {
            let names: Vec<_> = agents.primaries().map(|a| a.name.as_str()).collect();
            let there = match names.is_empty() {
                true => "there are none".to_string(),
                false => format!("there are: {}", names.join(", ")),
            };
            return Err(anyhow!("no primary agent {name:?}; {there}"));
        };
        // Its own model, else act's, which setup picked as the mode is act.
        if let Some(model) = agent.model.clone() {
            session.set_model(provider.qualify(&model));
        }
        session.set_agent(Some(name));
    }
    // The model is final now: take the configured compaction policy and the
    // limits its listing names, so a long turn compacts itself.
    apply_context(&mut session, &config, &provider).await;
    let provider: Arc<dyn Provider> = Arc::new(provider);
    // The read tool keeps images beside the save, as the chat's does.
    let store = Store::open();
    if let Ok(store) = &store {
        session.images = Some(store.images_dir(session.id));
    }
    let cwd = session.cwd.clone();
    let post_write = post_write(&config);
    let lsp = post_write.lsp().clone();
    // Without a front-end, a subagent's answer has nothing to wake the
    // model, so the task tool waits for it.
    let subagents = Subagents::default();
    let tools = crate::tools(&config, post_write, provider.clone(), subagents.clone());
    // `/name args` runs a skill, as in the chat.
    let prompt = match nth_context::skills::parse(&prompt, &session.context().skills) {
        Some((skill, args)) => skill
            .invoke(args, &cwd)
            .await
            .map_err(|e| anyhow!("could not run the skill: {e}"))?,
        None => prompt,
    };

    let interrupt = Interrupt::default();
    let watcher = tokio::spawn({
        let interrupt = interrupt.clone();
        async move {
            if interrupts(tokio::signal::ctrl_c, interrupt).await.is_ok() {
                // 128 + SIGINT, what a shell reports for a process ctrl+c
                // killed.
                std::process::exit(130);
            }
        }
    });
    let ran = async {
        let started = Instant::now();
        let spends = session.usage.spends().len();
        let worked = work(
            &mut session,
            prompt,
            provider.as_ref(),
            &tools,
            &subagents,
            store,
            &interrupt,
        )
        .await;
        // The turn is over and saved, so a ctrl+c from here on exits at once
        // instead of waiting for the summary.
        interrupt.cancel.cancel();
        lsp.shutdown().await;
        let (turn, printer) = worked?;
        let (icon, outcome) = match &turn {
            Ok(()) => (icons().ok.green().bold().to_string(), None),
            Err(nth_session::Error::Interrupted) => (
                icons().warn.yellow().bold().to_string(),
                Some("interrupted"),
            ),
            // A turn that failed on its own says why; how far it got is
            // in the saved session.
            Err(_) => return turn.map_err(Into::into),
        };

        let elapsed = started.elapsed();
        let spent = usage::total(session.usage.since(spends));
        let price = match spent.steps {
            0 => None,
            _ => price(provider.as_ref(), session.usage.since(spends)).await,
        };
        let line = [
            outcome.map(str::to_string),
            Some(session.model.clone()),
            Some(format!("{} tool calls", printer.tool_calls)),
            Some(summary(spent, price)),
            Some(format!("{:.1}s", elapsed.as_secs_f64())),
        ];
        let line: Vec<_> = line.into_iter().flatten().collect();
        eprintln!("{icon} {}", line.join(" · ").dimmed());
        // Still an error, so a script sees the run did not finish: main
        // exits 130 on it without reporting it again.
        turn.map_err(Into::into)
    }
    .await;
    watcher.abort();
    ran
}

/// What ctrl+c reaches in a run: `cancel` stops its turn, and is cancelled
/// too once the run is saved; `ended` is cancelled as soon as the turn
/// returns, whatever its outcome.
#[derive(Clone, Default)]
struct Interrupt {
    cancel: CancellationToken,
    ended: CancellationToken,
}

/// Waits out the run's interrupts, each one `signal` resolving: the first
/// cancels the turn, which ends with every call answered and is saved; the
/// next, or any once `cancel` was cancelled because the run is saved,
/// returns so the caller can exit at once. Returns the error when no
/// interrupt can be heard, leaving ctrl+c to the OS.
async fn interrupts<S, F>(mut signal: S, interrupt: Interrupt) -> std::io::Result<()>
where
    S: FnMut() -> F,
    F: Future<Output = std::io::Result<()>>,
{
    loop {
        signal().await?;
        if interrupt.cancel.is_cancelled() {
            return Ok(());
        }
        // One landing after the turn finished on its own interrupts
        // nothing: the save goes on and the run reports as it ended.
        if !interrupt.ended.is_cancelled() {
            eprintln!(
                // On a line of its own, not mid-way through streamed output.
                "\n{} interrupted; saving the session (ctrl+c again to quit now)",
                icons().warn.yellow().bold()
            );
        }
        interrupt.cancel.cancel();
    }
}

/// Works on `prompt` until the turn ends or `interrupt` cancels it,
/// printing its events, then saves the session whatever the outcome.
/// Returns the turn's own result beside the printer that counted its calls.
async fn work(
    session: &mut Session,
    prompt: String,
    provider: &dyn Provider,
    tools: &[Box<dyn Tool>],
    subagents: &Subagents,
    store: Result<Store, store::Error>,
    interrupt: &Interrupt,
) -> Result<(Result<(), nth_session::Error>, render::Printer)> {
    let (tx, mut rx) = mpsc::channel(256);
    let cwd = session.cwd.clone();
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
            provider,
            tools,
            &FrontEnd::default(),
            &tx,
            &interrupt.cancel,
        )
        .await;
    interrupt.ended.cancel();
    drop(tx);
    // Cancelled subagents record what they spent as their turns end, which
    // may be after the parent's; one that will not stop is not worth a hung
    // exit.
    if interrupt.cancel.is_cancelled() {
        let ended = async {
            while subagents.running() > 0 {
                tokio::time::sleep(SUBAGENT_POLL).await;
            }
        };
        let _ = tokio::time::timeout(SUBAGENTS_WAIT, ended).await;
    }
    session.usage.extend(subagents.take_spent());
    let printer = printer.await;
    if let Ok(printer) = &printer {
        printer.finish();
    }
    // Saved even when the turn failed or was interrupted, so `nth -c` can
    // pick it up. Not saving is worth a warning, not a failed run.
    let saved = match store {
        Ok(store) => store.save(session).await,
        Err(e) => Err(e),
    };
    match saved {
        Err(e) => eprintln!("{} session not saved: {e}", icons().warn.yellow().bold()),
        Ok(()) => {
            if let Err(e) = session.acknowledge_curios() {
                eprintln!(
                    "{} Curios feedback not acknowledged: {e}",
                    icons().warn.yellow().bold()
                );
            }
        }
    }
    let printer = printer.context("printer task failed")?;
    Ok((turn, printer))
}

/// What the run cost at the catalogue's prices; `None` when the listing
/// is slow or knows none of the models.
async fn price(provider: &dyn Provider, spends: &[Spend]) -> Option<Price> {
    let listing = crate::usage::listing(provider).await?;
    usage::price_spends(spends, &|model| listing.cost_of(model))
}

/// `12 steps · 1.2M in · 82% cached · 40k out · $3.10`; the cache share
/// and the price are left out when unknown.
fn summary(spent: Total, price: Option<Price>) -> String {
    let tokens = spent.tokens;
    let mut parts = vec![steps(spent.steps), format!("{} in", short(tokens.input))];
    if let Some(share) = tokens.cached_share() {
        parts.push(format!("{:.0}% cached", share * 100.0));
    }
    parts.push(format!("{} out", short(tokens.output)));
    parts.extend(price.map(|price| price.to_string()));
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use futures::{
        FutureExt, StreamExt,
        future::BoxFuture,
        stream::{self, BoxStream},
    };
    use nth_protocol::{
        BoxError, INTERRUPTED, Listing, Message, Request, StreamEvent, ToolCall, ToolContext,
        ToolResult, ToolSpec, Usage, failed,
    };

    use super::*;

    /// Asks for one `stall` call in every reply.
    struct Stalling;

    impl Provider for Stalling {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            async { Ok(Listing::default()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            let call = StreamEvent::ToolCall(ToolCall {
                id: "1".into(),
                name: "stall".into(),
                arguments: "{}".into(),
            });
            async move { Ok(stream::iter([Ok(call)]).boxed()) }.boxed()
        }
    }

    /// Presses ctrl+c, as far as the run can tell, then never finishes.
    struct Stall(CancellationToken);

    impl Tool for Stall {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "stall",
                description: String::new(),
                parameters: serde_json::json!({}),
            }
        }

        fn call<'a>(
            &'a self,
            _: serde_json::Value,
            _: &'a ToolContext,
        ) -> BoxFuture<'a, ToolResult> {
            self.0.cancel();
            std::future::pending().boxed()
        }
    }

    #[tokio::test]
    async fn an_interrupted_run_is_saved() {
        let dir = tempfile::tempdir().expect("temp dir");
        let sessions = dir.path().join("sessions");
        let mut session = Session::new("stalling", dir.path().to_path_buf());
        let interrupt = Interrupt::default();
        let tools: Vec<Box<dyn Tool>> = vec![Box::new(Stall(interrupt.cancel.clone()))];
        let subagents = Subagents::default();

        let worked = work(
            &mut session,
            "wait".into(),
            &Stalling,
            &tools,
            &subagents,
            Ok(Store::at(&sessions)),
            &interrupt,
        );
        let (turn, printer) = tokio::time::timeout(Duration::from_secs(10), worked)
            .await
            .expect("the interrupted turn ends")
            .expect("the printer ends");

        assert!(matches!(turn, Err(nth_session::Error::Interrupted)));
        assert_eq!(printer.tool_calls, 1);
        assert!(interrupt.ended.is_cancelled());
        let saved = Store::at(&sessions)
            .load(session.id)
            .await
            .expect("the session was saved");
        assert!(
            saved
                .messages
                .iter()
                .any(|m| matches!(m, Message::User(text) if text.contains("wait")))
        );
        assert!(matches!(
            saved.messages.last(),
            Some(Message::ToolResult { call_id, content, .. })
                if call_id == "1" && *content == failed(INTERRUPTED)
        ));
    }

    #[tokio::test]
    async fn the_first_interrupt_cancels_the_turn_and_the_next_returns() {
        let signals = Arc::new(tokio::sync::Semaphore::new(0));
        let signal = {
            let signals = signals.clone();
            move || {
                let signals = signals.clone();
                async move {
                    signals.acquire().await.expect("open").forget();
                    Ok(())
                }
            }
        };
        let interrupt = Interrupt::default();
        let watching = tokio::spawn(interrupts(signal, interrupt.clone()));

        signals.add_permits(1);
        tokio::time::timeout(Duration::from_secs(10), interrupt.cancel.cancelled())
            .await
            .expect("the first interrupt cancels the turn");
        assert!(!watching.is_finished());

        signals.add_permits(1);
        tokio::time::timeout(Duration::from_secs(10), watching)
            .await
            .expect("the second interrupt returns")
            .expect("the watcher does not panic")
            .expect("signals are heard");
    }

    #[tokio::test]
    async fn an_interrupt_between_the_turn_and_its_save_lets_the_save_finish() {
        let interrupt = Interrupt::default();
        interrupt.ended.cancel();
        let signals = Arc::new(tokio::sync::Semaphore::new(1));
        let signal = move || {
            let signals = signals.clone();
            async move {
                signals.acquire().await.expect("open").forget();
                Ok(())
            }
        };

        let watching = tokio::spawn(interrupts(signal, interrupt.clone()));
        tokio::time::timeout(Duration::from_secs(10), interrupt.cancel.cancelled())
            .await
            .expect("the interrupt is taken");
        assert!(!watching.is_finished());
        watching.abort();
    }

    #[tokio::test]
    async fn an_interrupt_after_the_run_is_saved_returns_at_once() {
        let interrupt = Interrupt::default();
        interrupt.ended.cancel();
        interrupt.cancel.cancel();

        let watched = tokio::time::timeout(
            Duration::from_secs(10),
            interrupts(|| async { Ok(()) }, interrupt.clone()),
        )
        .await
        .expect("returns on the first interrupt");

        assert!(watched.is_ok());
    }

    #[test]
    fn summary_names_the_cache_share_and_price_only_when_known() {
        let mut spent = Total {
            steps: 12,
            tokens: Usage {
                input: 1_200_000,
                output: 40_000,
                cache_read: Some(984_000),
                cache_write: None,
                reasoning: None,
            },
        };
        let price = Price {
            dollars: 3.1,
            partial: false,
        };
        assert_eq!(
            summary(spent, Some(price)),
            "12 steps · 1.2M in · 82% cached · 40k out · $3.10"
        );
        spent.tokens.cache_read = None;
        assert_eq!(summary(spent, None), "12 steps · 1.2M in · 40k out");
    }
}
