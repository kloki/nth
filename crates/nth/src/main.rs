mod render;

use std::{
    io::{IsTerminal, Write},
    process::ExitCode,
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use nth_llm::chat_completions::ChatClient;
use nth_protocol::{ModelInfo, Provider};
use nth_session::{CancellationToken, Session};
use owo_colors::OwoColorize;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "nth", version, about = "A coding harness, for the N-th time")]
struct Cli {
    /// Without a subcommand, nth opens the interactive chat.
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    endpoint: Endpoint,
}

#[derive(Subcommand)]
enum Command {
    /// Send one prompt and let the agent work until it answers
    Run {
        prompt: String,
        #[command(flatten)]
        endpoint: Endpoint,
    },
    /// List the models the endpoint serves that nth can talk to
    Models {
        /// Print JSON lines, the default when stdout is not a terminal
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        endpoint: Endpoint,
    },
}

#[derive(Args)]
struct Endpoint {
    #[arg(long, env = "NTH_MODEL", default_value = "glm-5.3")]
    model: String,
    #[arg(
        long,
        env = "NTH_BASE_URL",
        default_value = "https://opencode.ai/zen/go/v1"
    )]
    base_url: String,
}

/// A fresh session in the working directory and the client it talks through.
fn setup(endpoint: Endpoint) -> Result<(Session, ChatClient)> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let session = Session::new(endpoint.model, cwd);
    let provider = ChatClient::new(endpoint.base_url, api_key()?, session.id.to_string());
    Ok((session, provider))
}

fn api_key() -> Result<String> {
    std::env::var("OPENCODE_GO_API_KEY").context("OPENCODE_GO_API_KEY not set")
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None => chat(cli.endpoint).await,
        Some(Command::Run { prompt, endpoint }) => run(prompt, endpoint).await,
        Some(Command::Models { json, endpoint }) => models(json, endpoint).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} {:#}", "✗".red().bold(), e.red());
            ExitCode::FAILURE
        }
    }
}

async fn chat(endpoint: Endpoint) -> Result<()> {
    let (session, provider) = setup(endpoint)?;
    nth_tui::run(session, Arc::new(provider), Arc::new(nth_tools::all())).await
}

async fn run(prompt: String, endpoint: Endpoint) -> Result<()> {
    let (mut session, provider) = setup(endpoint)?;
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
    let turn = session
        .prompt(prompt, &provider, &tools, &tx, &CancellationToken::new())
        .await;
    drop(tx);
    let printer = printer.await.context("printer task failed")?;
    printer.finish();
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

async fn models(json: bool, endpoint: Endpoint) -> Result<()> {
    // Listing needs no conversation, so no session id to route on.
    let provider = ChatClient::new(endpoint.base_url, api_key()?, String::new());
    let models = provider.models().await.map_err(|e| anyhow!(e))?;

    let mut out = std::io::stdout().lock();
    if json || !out.is_terminal() {
        for model in &models {
            writeln!(out, "{}", serde_json::to_string(model)?)?;
        }
        return Ok(());
    }

    let width = models.iter().map(|m| m.id.len()).max().unwrap_or(0);
    for model in &models {
        let limits = limits(model).dimmed().to_string();
        if model.id == endpoint.model {
            writeln!(
                out,
                "{} {:width$}  {limits}",
                "→".cyan().bold(),
                model.id.bold()
            )?;
        } else {
            writeln!(out, "  {:width$}  {limits}", model.id)?;
        }
    }
    Ok(())
}

fn limits(model: &ModelInfo) -> String {
    let tokens = |n: u64| match n {
        1_000_000.. => format!("{}M", n / 1_000_000),
        _ => format!("{}k", n / 1_000),
    };
    [
        model.context.map(|n| format!("{} ctx", tokens(n))),
        model.output.map(|n| format!("{} out", tokens(n))),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ")
}
