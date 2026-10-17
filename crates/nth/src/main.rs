//! The `nth` binary: the command line, and what every subcommand shares.
//! Each subcommand is its own module.

mod agents;
mod chat;
mod config;
mod connect;
mod formatters;
mod lsp;
mod models;
mod notify;
mod run;
mod session;
mod skills;
mod usage;
mod work;

use std::{path::PathBuf, process::ExitCode, sync::Arc};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use config::Config;
use nth_context::Paths;
use nth_icons::icons;
use nth_llm::{Credentials, EndpointConfig, Providers, Unavailable};
use nth_protocol::{Listing, Llm, Mode, Provider, SubagentLlm};
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
        None => match load(cli.config.as_deref(), cli.model) {
            Ok(config) => chat::run(cli.resume, config).await,
            Err(e) => Err(e),
        },
        Some(command) => dispatch(command, cli.config, cli.model).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} {:#}", icons().fail.red().bold(), e.red());
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
        Command::Usage { session, all, json } => usage::run(session, all, json, config()?).await,
        Command::Work { session, json } => work::run(session, json).await,
        Command::Session { command } => session::run(command).await,
        Command::Connect {
            status,
            logout,
            json,
        } => connect::run(status, logout, json).await,
    }
}

/// A fresh session in `mode` in the working directory, on that mode's
/// model, with its instruction files read, and the providers it talks
/// through. A mode without a model gets one picked from what the
/// providers list, written into `config` so the caller sees it too.
async fn setup(config: &mut Config, paths: &Paths, mode: Mode) -> Result<(Session, Providers)> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let providers = providers(config)?;
    // The model above fills in for every profile slot that has none, and
    // both modes when the config lists no profiles at all.
    if config.needs_model() {
        config.model = random_model(&providers).await?;
        eprintln!(
            "no model set, picked {} (set model in the config or pass --model)",
            config.model
        );
    }
    let (model, effort) = config.llm_for(mode);
    let model = providers.qualify(&model);
    let mut session = Session::new(model, cwd.clone()).with_context(context(cwd, paths).await);
    session.mode = mode;
    session.effort = effort;
    session.max_steps = config.session.max_steps;
    session.subagent_llm = config.subagent_for().map(|(model, effort)| SubagentLlm {
        llm: Llm {
            model: providers.qualify(&model),
            effort,
        },
        ..SubagentLlm::default()
    });
    Ok((session, providers))
}

/// Sizes the session's compaction to its model: the `[context]` policy,
/// then the context window and output limit the provider's listing names
/// for the session's model. Both front-ends call it once the model is
/// final. A listing that fails changes no limits, so a fresh session keeps
/// both unknown and nothing compacts on its own.
pub(crate) async fn apply_context(session: &mut Session, config: &Config, provider: &dyn Provider) {
    session.compaction = config.context;
    set_model_limits(session, provider).await;
}

/// Sizes the session's compaction to its model: the context window and
/// output limit the providers' listing names for it, and for what its
/// subagents run on. A listing that fails
/// changes nothing, so a fresh session keeps both limits unknown and
/// nothing compacts on its own.
pub(crate) async fn set_model_limits(session: &mut Session, provider: &dyn Provider) {
    let Ok(listing) = provider.models().await else {
        return;
    };
    if let Some((context_window, max_output)) = limits(&listing, &session.model) {
        session.set_limits(context_window, max_output);
    }
    if let Some(takes) = takes_images(&listing, &session.model) {
        session.takes_images = takes;
    }
    if let Some(subagent) = &mut session.subagent_llm
        && let Some((context_window, max_output)) = limits(&listing, &subagent.llm.model)
    {
        subagent.context_window = context_window;
        subagent.max_output = max_output;
    }
    if let Some(subagent) = &mut session.subagent_llm {
        subagent.takes_images = takes_images(&listing, &subagent.llm.model);
    }
}

/// Whether the listing says `model` takes images; `None` when it does not
/// name it.
fn takes_images(listing: &Listing, model: &str) -> Option<bool> {
    listing
        .models
        .iter()
        .find(|info| info.id == model)
        .map(|info| info.takes_images)
}

/// The context window and output limit the listing gives `model`; `None`
/// when the listing does not name it.
fn limits(listing: &Listing, model: &str) -> Option<(Option<u64>, Option<u64>)> {
    listing
        .models
        .iter()
        .find(|info| info.id == model)
        .map(|info| (info.context, info.output))
}

/// Any one of the models the providers list.
async fn random_model(providers: &Providers) -> Result<String> {
    let listing = providers
        .models()
        .await
        .map_err(|e| anyhow::anyhow!("no model set, and listing models failed: {e}"))?;
    pick(listing)
}

/// A model from `listing` at random; none listed is an error naming the
/// providers that could not be asked.
fn pick(mut listing: Listing) -> Result<String> {
    if listing.models.is_empty() {
        let failed: Vec<_> = listing
            .failed
            .iter()
            .map(|f| format!("{}: {}", f.origin.id, f.error))
            .collect();
        bail!(
            "no model set, and the providers list none to pick from{}",
            match failed.as_slice() {
                [] => String::new(),
                failed => format!(" ({})", failed.join("; ")),
            }
        );
    }
    let picked = fastrand::usize(..listing.models.len());
    Ok(listing.models.swap_remove(picked).id)
}

/// The configured providers whose keys are set, and ChatGPT, which runs
/// once you connect, even mid-run. Neither a key set nor ChatGPT connected
/// is an error naming every variable; so is a model, the config's or a
/// mode's, on a provider whose key is not set or that is not connected.
fn providers(config: &Config) -> Result<Providers> {
    let chatgpt = chatgpt()?;
    let mut endpoints = Vec::new();
    let mut unavailable = Vec::new();
    for (id, provider) in &config.provider {
        match std::env::var(&provider.api_key_env) {
            Ok(api_key) if !api_key.is_empty() => endpoints.push(EndpointConfig {
                id: id.clone(),
                name: provider.name.clone(),
                base_url: provider.base_url.clone(),
                credentials: Credentials::Key(api_key),
                models: provider.models.clone(),
            }),
            _ => unavailable.push(Unavailable {
                id: id.clone(),
                api_key_env: provider.api_key_env.clone(),
            }),
        }
    }
    if endpoints.is_empty() && !chatgpt.connected() {
        if config.provider.is_empty() {
            bail!(
                "no provider configured: run `nth connect` to use ChatGPT, or add a \
                 [provider.<id>] to the config (`nth init` writes one with OpenCode Go \
                 and Zen ready to uncomment)"
            );
        }
        // Providers may share a variable.
        let mut variables: Vec<_> = unavailable.iter().map(|u| u.api_key_env.as_str()).collect();
        variables.sort();
        variables.dedup();
        bail!(
            "no provider has its API key set: {}, or run `nth connect`",
            variables.join(", ")
        );
    }
    // Last, so a bare model goes to a configured provider first.
    endpoints.push(EndpointConfig::chatgpt(chatgpt));
    let providers = Providers::new(endpoints, unavailable, &config.model)
        .with_context(|| format!("model {} cannot run", config.model))?;
    for mode in [Mode::Plan, Mode::Act] {
        let (model, _) = config.llm_for(mode);
        let what = match config.profiles.first() {
            Some(profile) => format!("{mode:?} in profile {}", profile.name),
            None => format!("{mode:?} mode"),
        };
        providers
            .check(&model)
            .with_context(|| format!("model {model} for {what} cannot run"))?;
    }
    Ok(providers)
}

/// The ChatGPT login, kept in nth's `auth.json`.
fn chatgpt() -> Result<nth_auth::ChatGpt> {
    Ok(nth_auth::ChatGpt::new(nth_auth::Store::new()?)?)
}

/// Every tool the model gets: nth-tools' list, the task tool that hands
/// each subagent its share of the same list, task_stop, and the work
/// plan's tools.
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
    let stop = nth_session::TaskStop::new(subagents.clone());
    let work = nth_session::Work::new(
        provider.clone(),
        shared.clone(),
        subagents.clone(),
        config.work.limits(),
    );
    let task = nth_session::Task::new(provider, shared.clone(), subagents, config.task.limits());
    let mut tools: Vec<Box<dyn nth_protocol::Tool>> = shared
        .into_iter()
        .map(|tool| Box::new(tool) as Box<dyn nth_protocol::Tool>)
        .collect();
    tools.push(Box::new(task));
    tools.push(Box::new(stop));
    tools.extend(work.tools());
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

#[cfg(test)]
mod tests {
    use futures::{FutureExt, future::BoxFuture, stream::BoxStream};
    use nth_protocol::{BoxError, Failed, ModelInfo, Origin, Request, StreamEvent};

    use super::*;

    fn model(id: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: None,
            context: None,
            output: None,
            efforts: Vec::new(),
            origin: None,
            cost: None,
            takes_images: false,
        }
    }

    #[test]
    fn limits_come_from_the_listing_entry_of_the_model() {
        let listed = |id: &str, context: Option<u64>, output: Option<u64>| ModelInfo {
            context,
            output,
            ..model(id)
        };
        let listing = Listing::from(vec![
            listed("zen/a", Some(200_000), Some(64_000)),
            listed("go/c", Some(1_000_000), None),
        ]);

        assert_eq!(
            limits(&listing, "zen/a"),
            Some((Some(200_000), Some(64_000)))
        );
        assert_eq!(limits(&listing, "go/c"), Some((Some(1_000_000), None)));
        assert_eq!(limits(&listing, "not/listed"), None);
    }

    /// A provider whose listing names `models`; its stream is never asked
    /// for, as `set_model_limits` only lists.
    struct Listed(Vec<ModelInfo>);

    impl Provider for Listed {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            async move { Ok(Listing::from(self.0.clone())) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            async { Err("unused".into()) }.boxed()
        }
    }

    /// A provider whose listing fails, as an unreachable endpoint's does.
    struct Unlistable;

    impl Provider for Unlistable {
        fn models(&self) -> BoxFuture<'_, Result<Listing, BoxError>> {
            async { Err("no /models".into()) }.boxed()
        }

        fn stream<'a>(
            &'a self,
            _: Request<'a>,
        ) -> BoxFuture<'a, Result<BoxStream<'static, Result<StreamEvent, BoxError>>, BoxError>>
        {
            async { Err("unused".into()) }.boxed()
        }
    }

    fn listed(id: &str, context: Option<u64>, output: Option<u64>) -> ModelInfo {
        ModelInfo {
            context,
            output,
            ..model(id)
        }
    }

    #[tokio::test]
    async fn set_model_limits_puts_the_listed_window_and_output_on_the_session() {
        let provider = Listed(vec![listed("zen/a", Some(200_000), Some(64_000))]);
        let mut session = Session::new("zen/a", "/repo".into());

        set_model_limits(&mut session, &provider).await;

        assert_eq!(session.context_window, Some(200_000));
        assert_eq!(session.max_output, Some(64_000));
    }

    #[tokio::test]
    async fn set_model_limits_leaves_unknown_or_unlisted_models_alone() {
        // Neither a model the listing does not name...
        let unlisted = Listed(vec![listed("zen/b", Some(1_000), Some(100))]);
        let mut session = Session::new("zen/a", "/repo".into());
        set_model_limits(&mut session, &unlisted).await;
        assert_eq!(session.context_window, None);
        assert_eq!(session.max_output, None);

        // ...nor a listing that fails changes what was already known: a
        // fresh session keeps both unknown and nothing compacts on its own.
        let mut resumed = Session::new("zen/a", "/repo".into());
        resumed.set_limits(Some(100_000), Some(8_000));
        set_model_limits(&mut resumed, &Unlistable).await;
        assert_eq!(resumed.context_window, Some(100_000));
        assert_eq!(resumed.max_output, Some(8_000));
    }

    #[tokio::test]
    async fn a_context_config_reaches_a_session_with_the_listed_limits() {
        // Through the helper both `chat::run` and `run::run` call, so the
        // wiring they share is what this exercises.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            r#"
            model = "zen/a"

            [context]
            auto = false
            reserved = 30000
            preserve = 1000
            "#,
        )
        .expect("writes");
        let config = Config::load(Some(&path)).expect("parses");
        let provider = Listed(vec![listed("zen/a", Some(200_000), Some(64_000))]);
        let mut session = Session::new("zen/a", "/repo".into());

        apply_context(&mut session, &config, &provider).await;

        assert!(!session.compaction.auto);
        assert_eq!(session.compaction.reserved, 30_000);
        assert_eq!(session.compaction.preserve, 1_000);
        assert_eq!(session.context_window, Some(200_000));
        assert_eq!(session.max_output, Some(64_000));
    }

    #[test]
    fn picks_one_of_the_listed_models() {
        let ids = ["zen/a", "zen/b", "go/c"];
        let listing = Listing::from(ids.map(model).to_vec());
        for _ in 0..20 {
            let picked = pick(listing.clone()).expect("one is listed");
            assert!(ids.contains(&picked.as_str()), "{picked}");
        }
    }

    #[test]
    fn nothing_listed_names_the_providers_that_failed() {
        assert!(pick(Listing::default()).is_err());
        let listing = Listing {
            models: Vec::new(),
            failed: vec![Failed {
                origin: Origin {
                    id: "zen".into(),
                    name: "Zen".into(),
                },
                error: "401".into(),
            }],
        };
        let error = pick(listing).expect_err("nothing to pick").to_string();
        assert!(error.contains("zen: 401"), "{error}");
    }
}
