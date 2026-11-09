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
mod job;
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
    io::{IsTerminal, stdin, stdout},
    sync::Arc,
};

use anyhow::{Result, bail};
use crossterm::style::Stylize;
use futures::future::BoxFuture;
use nth_context::Paths;
use nth_format::Formatters;
use nth_icons::icons;
use nth_lsp::Lsp;
use nth_protocol::{Provider, Tool};
use nth_session::{Inbox, Session, Store, Subagents, subagent::SubagentEvent};
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

/// Everything a session runs with in its repository: the session, where
/// nth looks for project files, its provider, the model's tools and the
/// shell for `!` commands, what checks the writes, what each kind of turn
/// runs on with the profiles ctrl+p cycles, what is worth telling you about
/// how it was prepared, and the config it was prepared from. The chat starts on
/// one and `/resume` swaps in another.
pub struct PreparedRuntime {
    pub session: Session,
    /// What the model's task and work tools post to, which the chat
    /// empties: the process's one, so `/resume` swapping runtimes keeps it.
    pub inbox: Inbox,
    pub paths: Paths,
    pub provider: Arc<dyn Provider>,
    pub model_tools: Arc<Vec<Box<dyn Tool>>>,
    pub shell: Arc<dyn Tool>,
    pub checkers: Checkers,
    pub selection: nth_config::Selection,
    pub warnings: Vec<String>,
    /// Where the chat reads its own sections from: `[chat]`, `[layout]`
    /// and `[notify]`.
    pub config: nth_config::Config,
}

/// What `/resume` prepares a saved session's runtime with, for its own
/// repository: what the chat started on came through the same steps.
pub type Reload = Arc<dyn Fn(Session) -> BoxFuture<'static, Result<PreparedRuntime>> + Send + Sync>;

/// What the chat runs with beyond a runtime: the accounts `/connect` signs
/// in to and how `/resume` prepares another runtime.
pub struct Settings {
    /// What `/connect` signs in to ChatGPT through.
    pub chatgpt: nth_auth::ChatGpt,
    /// What `/resume` prepares another session's repository runtime with;
    /// without one, a resumed session keeps the current runtime.
    pub loader: Option<Reload>,
}

/// Runs the chat on `runtime` until the user quits, saving its session and
/// any other it moves on to in `store` after every turn. The subagents the
/// task tool starts are the process's, whichever runtime is in, and report
/// over `subagent_rx`. The terminal is restored on every exit path,
/// including a panic, and the language servers of the runtime it ends on
/// are shut down.
pub async fn run(
    runtime: PreparedRuntime,
    subagents: Subagents,
    subagent_rx: mpsc::Receiver<SubagentEvent>,
    store: Store,
    Settings { chatgpt, loader }: Settings,
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
    let mut app = app::App::new(runtime)
        .with_subagents(subagents, subagent_rx)
        .with_store(store)
        .with_reload(loader)
        .with_history(history)
        .with_llm_usage(llm_usage)
        .with_chatgpt(chatgpt);
    let result = app.run(&mut terminal).await;
    terminal::restore();
    for note in &app.exit_notes {
        eprintln!("{} {note}", icons().warn.yellow().bold());
    }
    // After the terminal is back: the servers get a moment to leave on
    // their own before the process ends and kills them. `/resume` may have
    // replaced the pool the chat started with, so this is the one it ends on.
    if let Some(checkers) = app.checkers.take() {
        checkers.lsp.shutdown().await;
    }
    result
}
