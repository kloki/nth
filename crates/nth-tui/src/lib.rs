//! The interactive chat: the content panel, the chat history by default,
//! above the input panel and the status bar.

mod agent_picker;
mod app;
mod chat;
mod command;
mod connect;
mod context;
mod diagnostics;
mod file;
mod fuzzy;
mod git;
mod header;
mod hero;
mod history;
mod keymaps;
mod llm_picker;
mod mention;
mod monitor;
mod pager;
mod path;
mod plan;
mod popup;
mod prompt;
mod question;
mod rich;
mod search;
mod session_picker;
mod settings;
mod spinner;
mod status;
mod subagent;
mod terminal;
mod theme;
mod usage;
mod work;

use std::{
    future::Future,
    io::{IsTerminal, stdin, stdout},
    pin::Pin,
    sync::Arc,
};

use anyhow::{Result, bail};
pub use app::{Llm, ModeLlms, Profile};
use nth_context::Paths;
use nth_format::Formatters;
use nth_lsp::Lsp;
use nth_notify::Notifier;
use nth_protocol::{Provider, Tool};
use nth_session::{Session, Store, Subagents, subagent::SubagentEvent};
pub use settings::ChatSettings;
use tokio::sync::mpsc;

/// What the tools reach for besides themselves: the same language servers
/// and formatters the tools use to check their writes, and whether bash
/// passes the model's commands through rtk. The status bar shows the
/// servers' states, and the diagnostics tab what applies to the project.
#[derive(Clone)]
pub struct Checkers {
    pub lsp: Lsp,
    pub formatters: Arc<Formatters>,
    /// `[tools.bash] rtk`.
    pub rtk: bool,
}

/// What the chat runs commands with: the model's tools, the shell for the
/// commands you type after `!`, and the subagents the task tool among the
/// tools starts, with where they report.
pub struct Tools {
    pub model: Arc<Vec<Box<dyn Tool>>>,
    pub shell: Arc<dyn Tool>,
    pub subagents: Subagents,
    pub subagent_rx: mpsc::Receiver<SubagentEvent>,
}

/// A complete repository-specific runtime, prepared before switching sessions.
pub struct PreparedRuntime {
    pub session: Session,
    pub paths: Paths,
    pub provider: Arc<dyn Provider>,
    pub model_tools: Arc<Vec<Box<dyn Tool>>>,
    pub shell: Arc<dyn Tool>,
    pub checkers: Checkers,
    pub mode_llms: ModeLlms,
    pub profiles: Vec<Profile>,
    pub warnings: Vec<String>,
}

/// Prepare a saved session and its repository runtime using the shared subagent registry.
pub trait RuntimeLoader: Send + Sync {
    fn prepare(
        &self,
        session: Session,
    ) -> Pin<Box<dyn Future<Output = Result<PreparedRuntime>> + Send + '_>>;
}

/// What the config decided: where nth looks for project files, the model
/// and effort each mode runs on, the profiles ctrl+p cycles, where
/// notifications go, how the chat shows what the model did, and the
/// accounts `/connect` signs in to.
pub struct Settings {
    pub paths: Paths,
    pub mode_llms: ModeLlms,
    /// The `[[profiles]]` in config order; the chat starts on the first,
    /// and empty means there are none to cycle.
    pub profiles: Vec<Profile>,
    pub notifier: Notifier,
    pub chat: ChatSettings,
    /// The terminal width from which the chat and another tab show side by
    /// side; 0 never splits.
    pub split_width: u16,
    /// What `/connect` signs in to ChatGPT through.
    pub chatgpt: nth_auth::ChatGpt,
    /// What `/resume` prepares another session's repository runtime with;
    /// without one, a resumed session keeps the current runtime.
    pub loader: Option<Arc<dyn RuntimeLoader>>,
}

/// Runs the chat until the user quits, saving `session` and any other it
/// moves on to in `store` after every turn. Each mode runs on its model in
/// `mode_llms`. The terminal is restored on every exit path, including a
/// panic, and the language servers of the runtime it ends on are shut down.
pub async fn run(
    session: Session,
    provider: Arc<dyn Provider>,
    tools: Tools,
    checkers: Checkers,
    store: Store,
    Settings {
        paths,
        mode_llms,
        profiles,
        notifier,
        chat,
        split_width,
        chatgpt,
        loader,
    }: Settings,
) -> Result<()> {
    // Without this check, piped or tty-less runs would write setup escape
    // codes into the pipe and then fail on raw mode.
    if !stdin().is_terminal() || !stdout().is_terminal() {
        bail!("the interactive chat needs a terminal; use `nth run` for scripted use");
    }
    // Without a data directory, prompts are still recalled for this run.
    let history = match history::History::path() {
        Ok(path) => history::History::load(path).await,
        Err(_) => history::History::default(),
    };
    let llm_usage = match llm_picker::usage::LlmUsage::path() {
        Ok(path) => llm_picker::usage::LlmUsage::load(path).await,
        Err(_) => llm_picker::usage::LlmUsage::default(),
    };
    let mut terminal = terminal::enter()?;
    let mut app = app::App::new(session, provider, tools.model)
        .with_shell(tools.shell)
        .with_subagents(tools.subagents, tools.subagent_rx)
        .with_store(store)
        .with_paths(paths)
        .with_runtime_loader(loader)
        .with_history(history)
        .with_llm_usage(llm_usage)
        .with_checkers(checkers)
        .with_mode_llms(mode_llms)
        .with_profiles(profiles)
        .with_notifier(notifier)
        .with_chat_settings(chat)
        .with_split_width(split_width)
        .with_chatgpt(chatgpt);
    let result = app.run(&mut terminal).await;
    terminal::restore();
    // After the terminal is back: the servers get a moment to leave on
    // their own before the process ends and kills them. `/resume` may have
    // replaced the pool the chat started with, so this is the one it ends on.
    if let Some(checkers) = app.checkers.take() {
        checkers.lsp.shutdown().await;
    }
    result
}
