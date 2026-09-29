mod render;

use std::{process::ExitCode, time::Instant};

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
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Send one prompt and let the agent work until it answers
    Run {
        prompt: String,
        #[arg(long, env = "NTH_MODEL", default_value = "glm-5.3")]
        model: String,
        #[arg(
            long,
            env = "NTH_BASE_URL",
            default_value = "https://opencode.ai/zen/go/v1"
        )]
        base_url: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Run {
            prompt,
            model,
            base_url,
        } => run(prompt, model, base_url).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} {:#}", "✗".red().bold(), e.red());
            ExitCode::FAILURE
        }
    }
}

async fn run(prompt: String, model: String, base_url: String) -> Result<()> {
    let api_key = std::env::var("OPENCODE_GO_API_KEY").context("OPENCODE_GO_API_KEY not set")?;
    let cwd = std::env::current_dir().context("no working directory")?;
    let mut session = Session::new(model.clone(), cwd.clone());
    let provider = ChatClient::new(base_url, api_key, model, session.id.to_string());
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
