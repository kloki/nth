//! The `nth` binary: the command line, and what every subcommand shares.
//! Each subcommand is its own module.

mod agents;
mod chat;
mod config;
mod formatters;
mod lsp;
mod models;
mod notify;
mod run;
mod skills;

use std::{path::PathBuf, process::ExitCode, sync::Arc};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use config::Config;
use nth_context::Paths;
use nth_llm::chat_completions::ChatClient;
use nth_protocol::Mode;
use nth_session::Session;
use owo_colors::OwoColorize;

#[derive(Parser)]
#[command(name = "nth", version, about = "A coding harness, for the N-th time")]
struct Cli {
    /// Config file to use instead of ~/.config/nth/config.toml
    #[arg(long, global = true, env = "NTH_CONFIG")]
    config: Option<PathBuf>,
    /// Without a subcommand, nth opens the interactive chat.
    #[command(subcommand)]
    command: Option<Command>,
    /// Pick up the most recently used session instead of starting a new one
    #[arg(short = 'c', long = "continue")]
    resume: bool,
    /// Global, so `nth run --model x` and `NTH_MODEL=x nth config` mean the
    /// same as `nth --model x`: one place applies them for every subcommand.
    #[command(flatten)]
    endpoint: Endpoint,
}

#[derive(Subcommand)]
enum Command {
    /// Send one prompt and let the agent work until it answers
    Run {
        prompt: String,
        /// plan only writes a plan file; act may change anything
        #[arg(long, default_value = "act", value_parser = ["plan", "act"])]
        mode: String,
    },
    /// List the models the endpoint serves that nth can talk to
    Models {
        /// Print JSON lines, the default when stdout is not a terminal
        #[arg(long)]
        json: bool,
    },
    /// List the skills nth finds for the working directory
    Skills {
        /// Print JSON lines, the default when stdout is not a terminal
        #[arg(long)]
        json: bool,
    },
    /// List the agents the model can delegate to for the working directory
    Agents {
        /// Print JSON lines, the default when stdout is not a terminal
        #[arg(long)]
        json: bool,
    },
    /// List the formatters and whether they apply to the working directory
    Formatters {
        /// Print JSON lines, the default when stdout is not a terminal
        #[arg(long)]
        json: bool,
    },
    /// List the language servers and where they would run for the working directory
    Lsp {
        /// Print JSON lines, the default when stdout is not a terminal
        #[arg(long, global = true)]
        json: bool,
        #[command(subcommand)]
        command: Option<lsp::LspCommand>,
    },
    /// Print the config in use, with every default filled in
    Config,
    /// Send a sample notification through the configured backend
    Notify {
        /// Which notification to show
        #[arg(long, value_enum, default_value_t = notify::Sample::Done)]
        event: notify::Sample,
        /// Print JSON, the default when stdout is not a terminal
        #[arg(long)]
        json: bool,
    },
}

/// Overrides for the `[provider]` section of the config.
#[derive(Args)]
struct Endpoint {
    #[arg(long, global = true, env = "NTH_MODEL")]
    model: Option<String>,
    #[arg(long, global = true, env = "NTH_BASE_URL")]
    base_url: Option<String>,
}

impl Endpoint {
    fn apply(self, config: &mut Config) {
        if let Some(model) = self.model {
            config.set_model(model);
        }
        if let Some(base_url) = self.base_url {
            config.provider.base_url = base_url;
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match Config::load(cli.config.as_deref()) {
        Err(e) => Err(e),
        Ok(mut config) => {
            cli.endpoint.apply(&mut config);
            match cli.command {
                None => chat::run(cli.resume, config).await,
                Some(command) => dispatch(command, cli.config, config).await,
            }
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} {:#}", "✗".red().bold(), e.red());
            ExitCode::FAILURE
        }
    }
}

async fn dispatch(command: Command, config_path: Option<PathBuf>, config: Config) -> Result<()> {
    match command {
        Command::Run { prompt, mode } => match mode.parse() {
            Ok(mode) => run::run(prompt, mode, config).await,
            Err(e) => Err(anyhow::anyhow!(e)),
        },
        Command::Models { json } => models::run(json, config).await,
        Command::Skills { json } => skills::run(json, &config).await,
        Command::Agents { json } => agents::run(json, &config).await,
        Command::Formatters { json } => formatters::run(json, &config).await,
        Command::Lsp { json, command } => lsp::run(command, json, &config).await,
        Command::Config => config::show(config_path, &config),
        Command::Notify { event, json } => notify::run(event, json, &config).await,
    }
}

/// A fresh session in `mode` in the working directory, on that mode's
/// model, with its instruction files read, and the client it talks through.
async fn setup(config: &Config, paths: &Paths, mode: Mode) -> Result<(Session, ChatClient)> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let provider = client(config)?;
    let (model, effort) = config.llm_for(mode);
    let mut session = Session::new(model, cwd.clone()).with_context(context(cwd, paths).await);
    session.mode = mode;
    session.effort = effort;
    session.max_steps = config.session.max_steps;
    Ok((session, provider))
}

fn client(config: &Config) -> Result<ChatClient> {
    let key_env = &config.provider.api_key_env;
    let api_key = std::env::var(key_env).with_context(|| format!("{key_env} not set"))?;
    Ok(ChatClient::new(config.provider.base_url.clone(), api_key)?)
}

/// Every tool the model gets: nth-tools' list, and the task tool that
/// hands each subagent its share of the same list.
fn tools(
    config: &Config,
    post_write: nth_tools::PostWrite,
    provider: Arc<dyn nth_protocol::Provider>,
    subagents: nth_session::Subagents,
) -> Vec<Box<dyn nth_protocol::Tool>> {
    let shared: Vec<Arc<dyn nth_protocol::Tool>> = nth_tools::all(&config.tools, post_write)
        .into_iter()
        .map(Arc::from)
        .collect();
    let task = nth_session::Task::new(
        provider,
        shared.clone(),
        subagents,
        config.session.max_steps,
    );
    let mut tools: Vec<Box<dyn nth_protocol::Tool>> = shared
        .into_iter()
        .map(|tool| Box::new(tool) as Box<dyn nth_protocol::Tool>)
        .collect();
    tools.push(Box::new(task));
    tools
}

/// Runs after every tool that writes a file. Its language servers are the
/// only ones this process starts: the read tool shares them.
fn post_write(config: &Config) -> nth_tools::PostWrite {
    nth_tools::PostWrite::new(
        Arc::new(nth_format::Formatters::new(&config.format)),
        nth_lsp::Lsp::new(&config.lsp),
    )
}

/// What applies to `cwd`. Problems with it are warned about, not fatal.
async fn context(cwd: PathBuf, paths: &Paths) -> Arc<nth_context::Context> {
    let context = nth_context::Context::load(cwd, paths.clone()).await;
    for warning in &context.warnings {
        eprintln!("{} {warning}", "!".yellow().bold());
    }
    Arc::new(context)
}
