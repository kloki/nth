//! The `nth` binary: the command line, and what every subcommand shares.
//! Each subcommand is its own module.

mod agents;
mod chat;
mod config;
mod connect;
mod context;
mod curios;
mod formatters;
mod lsp;
mod memory;
mod models;
mod notify;
mod run;
mod runtime;
mod session;
mod skills;
mod text;
mod usage;
mod work;

use std::{path::PathBuf, process::ExitCode, sync::Arc};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nth_context::Paths;
use nth_icons::icons;
use owo_colors::OwoColorize;
use runtime::Runtime;

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
    /// Publish and review repository-local static documents
    Curio(curios::Args),
    /// Send one prompt and let the agent work until it answers
    Run {
        prompt: String,
        /// plan only writes a plan file; act may change anything
        #[arg(long, default_value = "act", value_parser = ["plan", "act"])]
        mode: String,
        /// Run as this primary agent in place of the mode (`nth agents`
        /// lists them), on its own model or else act's
        #[arg(long, conflicts_with = "mode")]
        agent: Option<String>,
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
    /// Show what a saved session spent: tokens, cache hits and price
    Usage {
        /// The session, by its id or the start of it; the latest by default
        session: Option<String>,
        /// Every saved session instead, per day and model
        #[arg(long, conflicts_with = "session")]
        all: bool,
        /// Print JSON, the default when stdout is not a terminal
        #[arg(long)]
        json: bool,
    },
    /// Preview the next request reconstructed from a saved session
    Context {
        /// The session, by its id or the start of it; the latest by default
        session: Option<String>,
        /// Print JSON, the default when stdout is not a terminal
        #[arg(long)]
        json: bool,
    },
    /// List or delete saved sessions
    Session {
        #[command(subcommand)]
        command: session::SessionCommand,
    },
    /// Show a saved session's work plan: its cycles, items and todo lists
    Work {
        /// The session, by its id or the start of it; the latest by default
        session: Option<String>,
        /// Print JSON, the default when stdout is not a terminal
        #[arg(long)]
        json: bool,
    },
    /// List what the model saved in the project's memory, .nth/memory/
    Memory {
        /// Print JSON lines, the default when stdout is not a terminal
        #[arg(long)]
        json: bool,
    },
    /// Sign in to ChatGPT, so its models run on your subscription
    Connect {
        /// Only say whether ChatGPT is connected
        #[arg(long, conflicts_with = "logout")]
        status: bool,
        /// Sign out of ChatGPT instead
        #[arg(long)]
        logout: bool,
        /// With --status, print JSON, the default when stdout is not a terminal
        #[arg(long, requires = "status")]
        json: bool,
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
        None => chat::start(cli.resume, cli.config, cli.model).await,
        Some(command) => dispatch(command, cli.config, cli.model).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => failure(&e),
    }
}

/// How a command that failed exits: an interrupted run has printed its
/// summary as the one report and exits as a shell reports a process ctrl+c
/// killed (128 + SIGINT); anything else says why and exits 1.
fn failure(e: &anyhow::Error) -> ExitCode {
    if let Some(nth_session::Error::Interrupted) = e.downcast_ref() {
        return ExitCode::from(130);
    }
    eprintln!("{} {:#}", icons().fail.red().bold(), e.red());
    ExitCode::FAILURE
}

async fn dispatch(
    command: Command,
    config_path: Option<PathBuf>,
    model: Option<String>,
) -> Result<()> {
    // Each command loads the config itself, so init can run without one:
    // the file may not exist yet, or be the broken one --force replaces.
    let runtime = Runtime::new(config_path, model);
    let config = || runtime.load(&std::env::current_dir().context("no working directory")?);
    match command {
        Command::Curio(args) => curios::run(args).await,
        Command::Run {
            prompt,
            mode,
            agent,
        } => match mode.parse() {
            Ok(mode) => run::run(prompt, mode, agent, config()?, &runtime).await,
            Err(e) => Err(anyhow::anyhow!(e)),
        },
        Command::Models { json } => models::run(json, config()?, &runtime).await,
        Command::Skills { json } => skills::run(json, runtime.paths(&config()?)).await,
        Command::Agents { json } => agents::run(json, runtime.paths(&config()?)).await,
        Command::Formatters { json } => formatters::run(json, &config()?).await,
        Command::Lsp { json, command } => lsp::run(command, json, &config()?).await,
        Command::Config => config::show(&runtime, &config()?),
        Command::Init { force } => config::init(&runtime, force),
        Command::Notify { event, json } => notify::run(event, json, &config()?).await,
        Command::Usage { session, all, json } => {
            usage::run(session, all, json, config()?, &runtime).await
        }
        Command::Context { session, json } => context::run(session, json, &runtime).await,
        Command::Work { session, json } => work::run(session, json).await,
        Command::Memory { json } => memory::run(json, &config()?),
        Command::Session { command } => session::run(command).await,
        Command::Connect {
            status,
            logout,
            json,
        } => connect::run(status, logout, json, runtime.chatgpt()?).await,
    }
}

/// What applies to `cwd`. Problems with it are warned about, not fatal.
async fn context(cwd: PathBuf, paths: &Paths) -> Arc<nth_context::Context> {
    let context = nth_context::Context::load(cwd, paths.clone()).await;
    for warning in &context.warnings {
        eprintln!("{} {warning}", icons().warn.yellow().bold());
    }
    Arc::new(context)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_interrupted_run_exits_130_and_other_failures_1() {
        let interrupted = anyhow::Error::from(nth_session::Error::Interrupted);
        assert_eq!(failure(&interrupted), ExitCode::from(130));
        assert_eq!(failure(&anyhow::anyhow!("boom")), ExitCode::FAILURE);
    }

    #[test]
    fn context_accepts_a_session_prefix_and_json() {
        let cli = Cli::try_parse_from(["nth", "context", "abc", "--json"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Context { session: Some(id), json: true }) if id == "abc"
        ));
        let cli = Cli::try_parse_from(["nth", "context"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Context {
                session: None,
                json: false
            })
        ));
    }
}
