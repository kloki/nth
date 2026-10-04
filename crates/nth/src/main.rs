mod render;

use std::{
    io::{IsTerminal, Write},
    path::PathBuf,
    process::ExitCode,
    sync::Arc,
    time::Instant,
};

use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand};
use nth_context::Paths;
use nth_llm::chat_completions::ChatClient;
use nth_protocol::Provider;
use nth_session::{CancellationToken, Session, Store};
use owo_colors::OwoColorize;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "nth", version, about = "A coding harness, for the N-th time")]
struct Cli {
    /// Without a subcommand, nth opens the interactive chat.
    #[command(subcommand)]
    command: Option<Command>,
    /// Pick up the most recently used session instead of starting a new one
    #[arg(short = 'c', long = "continue")]
    resume: bool,
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
    #[arg(long, env = "NTH_MODEL", default_value = "deepseek-v4.1-flash")]
    model: String,
    #[arg(
        long,
        env = "NTH_BASE_URL",
        default_value = "https://opencode.ai/zen/go/v1"
    )]
    base_url: String,
}

/// A fresh session in the working directory, with its instruction files
/// read, and the client it talks through.
async fn setup(endpoint: Endpoint, paths: &Paths) -> Result<(Session, ChatClient)> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let provider = ChatClient::new(endpoint.base_url, api_key()?);
    let session = Session::new(endpoint.model, cwd.clone()).with_context(context(cwd, paths).await);
    Ok((session, provider))
}

/// What applies to `cwd`. Problems with it are warned about, not fatal.
async fn context(cwd: PathBuf, paths: &Paths) -> Arc<nth_context::Context> {
    let context = nth_context::Context::load(cwd, paths.clone()).await;
    for warning in &context.warnings {
        eprintln!("{} {warning}", "!".yellow().bold());
    }
    Arc::new(context)
}

fn api_key() -> Result<String> {
    std::env::var("OPENCODE_GO_API_KEY").context("OPENCODE_GO_API_KEY not set")
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None => chat(cli.resume, cli.endpoint).await,
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

/// With `resume`, the last session comes back as it was: its model, effort
/// and working directory win over the flags and where nth was started.
async fn chat(resume: bool, endpoint: Endpoint) -> Result<()> {
    let paths = Paths::from_env();
    let (mut session, provider) = setup(endpoint, &paths).await?;
    let store = Store::open()?;
    if resume {
        session = store
            .latest()
            .await?
            .context("no saved session to continue")?;
        let context = context(session.cwd.clone(), &paths).await;
        session.set_context(context);
    }
    nth_tui::run(
        session,
        Arc::new(provider),
        Arc::new(nth_tools::all()),
        store,
        paths,
    )
    .await
}

async fn run(prompt: String, endpoint: Endpoint) -> Result<()> {
    let (mut session, provider) = setup(endpoint, &Paths::from_env()).await?;
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
    // Saved even when the turn failed, so `nth -c` can pick it up. Not
    // saving is worth a warning, not a failed run.
    let saved = match Store::open() {
        Ok(store) => store.save(&session).await,
        Err(e) => Err(e),
    };
    if let Err(e) = saved {
        eprintln!("{} session not saved: {e}", "!".yellow().bold());
    }
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
    let provider = ChatClient::new(endpoint.base_url, api_key()?);
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
        let limits = model.limits().dimmed().to_string();
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
