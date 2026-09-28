use std::{io::Write, process::ExitCode, time::Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use nth_llm::chat_completions::{ChatClient, Message, Role};
use owo_colors::OwoColorize;

#[derive(Parser)]
#[command(name = "nth", version, about = "A coding harness, for the N-th time")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Send one prompt and stream the reply
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
    let client = ChatClient::new(base_url, api_key, model, uuid::Uuid::new_v4().to_string());
    let messages = [Message {
        role: Role::User,
        content: prompt,
    }];

    let started = Instant::now();
    let mut stream = client.stream(&messages).await?;
    let mut stdout = std::io::stdout().lock();
    let mut chars = 0;
    while let Some(delta) = stream.next().await {
        let delta = delta?;
        chars += delta.chars().count();
        stdout.write_all(delta.as_bytes())?;
        stdout.flush()?;
    }
    writeln!(stdout)?;

    let footer = format!(
        "{} · {chars} chars · {:.1}s",
        client.model(),
        started.elapsed().as_secs_f64()
    );
    eprintln!("{} {}", "✓".green().bold(), footer.dimmed());
    Ok(())
}
