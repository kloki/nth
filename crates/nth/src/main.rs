mod config;
mod lsp;
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
use config::Config;
use nth_context::Paths;
use nth_llm::chat_completions::ChatClient;
use nth_protocol::Provider;
use nth_session::{CancellationToken, Session, Store};
use owo_colors::OwoColorize;
use tokio::sync::mpsc;

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
    /// List the skills nth finds for the working directory
    Skills {
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
}

/// Overrides for the `[provider]` section of the config.
#[derive(Args)]
struct Endpoint {
    #[arg(long, env = "NTH_MODEL")]
    model: Option<String>,
    #[arg(long, env = "NTH_BASE_URL")]
    base_url: Option<String>,
}

impl Endpoint {
    fn apply(self, config: &mut Config) {
        if let Some(model) = self.model {
            config.provider.model = model;
        }
        if let Some(base_url) = self.base_url {
            config.provider.base_url = base_url;
        }
    }
}

/// A fresh session in the working directory, with its instruction files
/// read, and the client it talks through.
async fn setup(config: &Config, paths: &Paths) -> Result<(Session, ChatClient)> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let provider = client(config)?;
    let mut session = Session::new(config.provider.model.clone(), cwd.clone())
        .with_context(context(cwd, paths).await);
    session.max_steps = config.session.max_steps;
    Ok((session, provider))
}

fn client(config: &Config) -> Result<ChatClient> {
    let key_env = &config.provider.api_key_env;
    let api_key = std::env::var(key_env).with_context(|| format!("{key_env} not set"))?;
    Ok(ChatClient::new(config.provider.base_url.clone(), api_key))
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

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match Config::load(cli.config.as_deref()) {
        Err(e) => Err(e),
        Ok(mut config) => match cli.command {
            None => {
                cli.endpoint.apply(&mut config);
                chat(cli.resume, config).await
            }
            Some(Command::Run { prompt, endpoint }) => {
                endpoint.apply(&mut config);
                run(prompt, config).await
            }
            Some(Command::Models { json, endpoint }) => {
                endpoint.apply(&mut config);
                models(json, config).await
            }
            Some(Command::Skills { json }) => skills(json, &config).await,
            Some(Command::Formatters { json }) => formatters(json, &config).await,
            Some(Command::Lsp { json, command }) => lsp::run(command, json, &config).await,
            Some(Command::Config) => show_config(cli.config, &config),
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

/// With `resume`, the last session comes back as it was: its model, effort
/// and working directory win over the flags and where nth was started.
async fn chat(resume: bool, config: Config) -> Result<()> {
    let paths = config.paths();
    let (mut session, provider) = setup(&config, &paths).await?;
    let store = Store::open()?;
    if resume {
        session = store
            .latest()
            .await?
            .context("no saved session to continue")?;
        let context = context(session.cwd.clone(), &paths).await;
        session.set_context(context);
        session.max_steps = config.session.max_steps;
    }
    let post_write = post_write(&config);
    // From the same servers and formatters the tools use, so the status
    // bar shows what checks the writes.
    let checkers = nth_tui::Checkers {
        lsp: post_write.lsp().status(),
        format: post_write.formatters().clone(),
    };
    nth_tui::run(
        session,
        Arc::new(provider),
        Arc::new(nth_tools::all(&config.tools, post_write)),
        checkers,
        store,
        paths,
    )
    .await
}

async fn run(prompt: String, config: Config) -> Result<()> {
    let (mut session, provider) = setup(&config, &config.paths()).await?;
    let cwd = session.cwd.clone();
    let tools = nth_tools::all(&config.tools, post_write(&config));
    // `/name args` runs a skill, as in the chat.
    let prompt = match nth_context::skills::parse(&prompt, &session.context().skills) {
        Some((skill, args)) => skill
            .invoke(args, &cwd)
            .await
            .map_err(|e| anyhow!("could not run the skill: {e}"))?,
        None => prompt,
    };

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

/// Longest description shown in the terminal listing.
const DESCRIPTION_CHARS: usize = 72;

async fn skills(json: bool, config: &Config) -> Result<()> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let context = context(cwd, &config.paths()).await;

    let mut out = std::io::stdout().lock();
    if json || !out.is_terminal() {
        for skill in context.skills.iter() {
            let line = serde_json::json!({
                "name": skill.name,
                "description": skill.description,
                "path": skill.path,
                "source": skill.source.name(),
            });
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }

    if context.skills.is_empty() {
        writeln!(out, "{} {}", "→".cyan().bold(), "no skills found".dimmed())?;
        return Ok(());
    }
    let mut sources: Vec<_> = context.skills.iter().map(|s| s.source).collect();
    sources.sort();
    sources.dedup();
    let width = context
        .skills
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(0);
    for source in sources {
        writeln!(out, "{}", source.name().bold())?;
        let group: Vec<_> = context
            .skills
            .iter()
            .filter(|s| s.source == source)
            .collect();
        for (i, skill) in group.iter().enumerate() {
            let branch = if i + 1 == group.len() {
                "└─"
            } else {
                "├─"
            };
            let about = match &skill.description {
                Some(d) => shorten(d).dimmed().to_string(),
                None => "no description, so the model is not offered it"
                    .yellow()
                    .to_string(),
            };
            writeln!(
                out,
                "{} {:width$}  {about}",
                branch.dimmed(),
                skill.name.cyan()
            )?;
        }
    }
    Ok(())
}

/// The first line of `text`, cut to fit one terminal row.
fn shorten(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(DESCRIPTION_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

async fn formatters(json: bool, config: &Config) -> Result<()> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let status = nth_format::Formatters::new(&config.format)
        .status(&cwd)
        .await;

    let mut out = std::io::stdout().lock();
    if json || !out.is_terminal() {
        for formatter in &status {
            let line = match &formatter.command {
                Ok(command) => serde_json::json!({
                    "name": formatter.name,
                    "extensions": formatter.extensions,
                    "enabled": true,
                    "command": command,
                }),
                Err(reason) => serde_json::json!({
                    "name": formatter.name,
                    "extensions": formatter.extensions,
                    "enabled": false,
                    "reason": reason,
                }),
            };
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }

    let width = status.iter().map(|f| f.name.len()).max().unwrap_or(0);
    for formatter in &status {
        match &formatter.command {
            Ok(command) => writeln!(
                out,
                "{} {:width$}  {}",
                "✓".green().bold(),
                formatter.name.cyan(),
                command.join(" ").dimmed()
            )?,
            Err(reason) => writeln!(
                out,
                "{} {:width$}  {}",
                "✗".red(),
                formatter.name.dimmed(),
                reason.dimmed()
            )?,
        }
    }
    Ok(())
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

async fn models(json: bool, config: Config) -> Result<()> {
    let provider = client(&config)?;
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
        if model.id == config.provider.model {
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
