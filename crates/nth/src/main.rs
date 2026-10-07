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

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::Config;
use nth_context::Paths;
use nth_llm::{Endpoint, Providers, Unavailable};
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
    /// The model to run on, as provider/model; overrides the config's.
    /// Global, so `nth run --model x` and `NTH_MODEL=x nth config` mean the
    /// same as `nth --model x`: one place applies it for every subcommand.
    #[arg(long, global = true, env = "NTH_MODEL")]
    model: Option<String>,
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
    /// List the models the configured providers serve
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
    /// Write a config file with every default to ~/.config/nth/config.toml
    Init {
        /// Replace the file if there is one
        #[arg(long)]
        force: bool,
    },
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

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        None => match load(cli.config.as_deref(), cli.model) {
            Ok(config) => chat::run(cli.resume, config).await,
            Err(e) => Err(e),
        },
        Some(command) => dispatch(command, cli.config, cli.model).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} {:#}", "✗".red().bold(), e.red());
            ExitCode::FAILURE
        }
    }
}

/// The config, with `--model` applied over it.
fn load(path: Option<&std::path::Path>, model: Option<String>) -> Result<Config> {
    let mut config = Config::load(path)?;
    if let Some(model) = model {
        config.set_model(model);
    }
    Ok(config)
}

async fn dispatch(
    command: Command,
    config_path: Option<PathBuf>,
    model: Option<String>,
) -> Result<()> {
    // Each command loads the config itself, so init can run without one:
    // the file may not exist yet, or be the broken one --force replaces.
    let config = || load(config_path.as_deref(), model.clone());
    match command {
        Command::Run { prompt, mode } => match mode.parse() {
            Ok(mode) => run::run(prompt, mode, config()?).await,
            Err(e) => Err(anyhow::anyhow!(e)),
        },
        Command::Models { json } => models::run(json, config()?).await,
        Command::Skills { json } => skills::run(json, &config()?).await,
        Command::Agents { json } => agents::run(json, &config()?).await,
        Command::Formatters { json } => formatters::run(json, &config()?).await,
        Command::Lsp { json, command } => lsp::run(command, json, &config()?).await,
        Command::Config => config::show(config_path.clone(), &config()?),
        Command::Init { force } => config::init(config_path.clone(), force),
        Command::Notify { event, json } => notify::run(event, json, &config()?).await,
    }
}

/// A fresh session in `mode` in the working directory, on that mode's
/// model, with its instruction files read, and the providers it talks
/// through.
async fn setup(config: &Config, paths: &Paths, mode: Mode) -> Result<(Session, Providers)> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let providers = providers(config)?;
    let (model, effort) = config.llm_for(mode);
    let model = providers.qualify(&model);
    let mut session = Session::new(model, cwd.clone()).with_context(context(cwd, paths).await);
    session.mode = mode;
    session.effort = effort;
    session.max_steps = config.session.max_steps;
    Ok((session, providers))
}

/// The configured providers whose keys are set. None set is an error
/// naming every variable; so is a model, the config's or a mode's, on a
/// provider whose key is not.
fn providers(config: &Config) -> Result<Providers> {
    let mut endpoints = Vec::new();
    let mut unavailable = Vec::new();
    for (id, provider) in &config.provider {
        match std::env::var(&provider.api_key_env) {
            Ok(api_key) if !api_key.is_empty() => endpoints.push(Endpoint {
                id: id.clone(),
                name: provider.name.clone(),
                base_url: provider.base_url.clone(),
                api_key,
                models: provider.models.clone(),
            }),
            _ => unavailable.push(Unavailable {
                id: id.clone(),
                api_key_env: provider.api_key_env.clone(),
            }),
        }
    }
    if endpoints.is_empty() {
        let variables: Vec<_> = unavailable.iter().map(|u| u.api_key_env.as_str()).collect();
        bail!("no provider has its API key set: {}", variables.join(", "));
    }
    let providers = Providers::new(endpoints, unavailable, &config.model)
        .with_context(|| format!("model {} cannot run", config.model))?;
    for mode in [Mode::Plan, Mode::Act] {
        let (model, _) = config.llm_for(mode);
        providers
            .check(&model)
            .with_context(|| format!("model {model} for {mode:?} mode cannot run"))?;
    }
    Ok(providers)
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
