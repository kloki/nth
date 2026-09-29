mod config;
mod render;

use std::{path::PathBuf, process::ExitCode, time::Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use config::Config;
use nth_llm::chat_completions::ChatClient;
use nth_protocol::{Message, Provider, ToolContext};
use owo_colors::OwoColorize;
use tokio::sync::mpsc;

#[derive(Parser)]
#[command(name = "nth", version, about = "A coding harness, for the N-th time")]
struct Cli {
    /// Config file to use instead of ~/.config/nth/config.toml
    #[arg(long, global = true, env = "NTH_CONFIG")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Send one prompt and let the agent work until it answers
    Run {
        prompt: String,
        /// Overrides provider.model from the config
        #[arg(long, env = "NTH_MODEL")]
        model: Option<String>,
        /// Overrides provider.base_url from the config
        #[arg(long, env = "NTH_BASE_URL")]
        base_url: Option<String>,
    },
    /// Print the config in use, with every default filled in
    Config,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match Config::load(cli.config.as_deref()) {
        Err(e) => Err(e),
        Ok(mut config) => match cli.command {
            Command::Run {
                prompt,
                model,
                base_url,
            } => {
                if let Some(model) = model {
                    config.provider.model = model;
                }
                if let Some(base_url) = base_url {
                    config.provider.base_url = base_url;
                }
                run(prompt, config).await
            }
            Command::Config => show_config(cli.config, &config),
        },
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} {:#}", "✗".red().bold(), e.red());
            ExitCode::FAILURE
        }
    }
}

fn show_config(explicit: Option<PathBuf>, config: &Config) -> Result<()> {
    match explicit.or_else(Config::default_path) {
        Some(path) if path.exists() => {
            eprintln!("{} {}", "✓".green().bold(), path.display().dimmed())
        }
        Some(path) => eprintln!(
            "{} {}",
            "→".cyan().bold(),
            format!("no {}, using defaults", path.display()).dimmed()
        ),
        None => eprintln!(
            "{} {}",
            "→".cyan().bold(),
            "no home dir, using defaults".dimmed()
        ),
    }
    print!("{}", config.to_toml()?);
    Ok(())
}

async fn run(prompt: String, config: Config) -> Result<()> {
    let key_env = &config.provider.api_key_env;
    let api_key = std::env::var(key_env).with_context(|| format!("{key_env} not set"))?;
    let provider = ChatClient::new(
        config.provider.base_url,
        api_key,
        config.provider.model,
        uuid::Uuid::new_v4().to_string(),
    );
    let cwd = std::env::current_dir().context("no working directory")?;
    let mut messages = vec![
        Message::System(nth_session::system_prompt(provider.model(), &cwd)),
        Message::User(prompt),
    ];
    let tools = nth_tools::all(&config.tools);
    let ctx = ToolContext { cwd: cwd.clone() };

    let started = Instant::now();
    let (tx, mut rx) = mpsc::channel(256);
    let printer = tokio::spawn(async move {
        let mut out = render::Printer::new(cwd);
        while let Some(event) = rx.recv().await {
            out.event(&event);
        }
        out
    });
    let turn = nth_session::run_turn(
        &provider,
        &tools,
        &ctx,
        &mut messages,
        &tx,
        config.session.max_steps,
    )
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
            provider.model(),
            printer.tool_calls,
            started.elapsed().as_secs_f64()
        )
        .dimmed()
    );
    Ok(())
}
