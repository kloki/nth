mod render;

use std::{process::ExitCode, sync::Arc, time::Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nth_llm::chat_completions::ChatClient;
use nth_protocol::Provider;
use nth_session::Session;
use owo_colors::OwoColorize;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "nth", version, about = "A coding harness, for the N-th time")]
struct Cli {
    /// Without a subcommand, nth opens the interactive chat.
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, global = true, env = "NTH_MODEL", default_value = "glm-5.3")]
    model: String,
    #[arg(
        long,
        global = true,
        env = "NTH_BASE_URL",
        default_value = "https://opencode.ai/zen/go/v1"
    )]
    base_url: String,
}

#[derive(Subcommand)]
enum Command {
    /// Send one prompt and let the agent work until it answers
    Run { prompt: String },
}

#[tokio::main]
async fn main() -> ExitCode {
    match start(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} {:#}", "✗".red().bold(), e.red());
            ExitCode::FAILURE
        }
    }
}

async fn start(cli: Cli) -> Result<()> {
    let (session, provider) = setup(cli.model, cli.base_url)?;
    match cli.command {
        None => nth_tui::run(session, Arc::new(provider), Arc::new(nth_tools::all())).await,
        Some(Command::Run { prompt }) => run(prompt, session, provider).await,
    }
}

/// A fresh session in the working directory and the provider it talks to.
fn setup(model: String, base_url: String) -> Result<(Session, ChatClient)> {
    let api_key = std::env::var("OPENCODE_GO_API_KEY").context("OPENCODE_GO_API_KEY not set")?;
    let cwd = std::env::current_dir().context("no working directory")?;
    let session = Session::new(model.clone(), cwd);
    let provider = ChatClient::new(base_url, api_key, model, session.id.to_string());
    Ok((session, provider))
}

async fn run(prompt: String, mut session: Session, provider: ChatClient) -> Result<()> {
    let cwd = session.cwd.clone();
    let tools = nth_tools::all();

    let started = Instant::now();
    let (tx, mut rx) = mpsc::channel(256);
    let printer = tokio::spawn(async move {
        let mut out = render::Printer::new(cwd);
        while let Some(event) = rx.recv().await {
            out.event(&event);
        }
        out
    });
    let turn = session.prompt(prompt, &provider, &tools, &tx).await;
    drop(tx);
    let printer = printer.await.context("printer task failed")?;
    printer.finish();
    turn?;

    eprintln!(
        "{} {}",
        "✓".green().bold(),
        format!(
            "{} · {} tool calls · {:.1}s",
            provider.model(),
            printer.tool_calls,
            started.elapsed().as_secs_f64()
        )
        .dimmed()
    );
    Ok(())
}
