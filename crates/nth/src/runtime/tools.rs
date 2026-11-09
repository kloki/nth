//! What a session's model and the chat run commands with.

use std::sync::Arc;

use nth_config::Config;
use nth_protocol::{Provider, Tool};
use nth_session::{Inbox, Subagents};
use nth_tools::PostWrite;

/// Every tool the model gets: nth-tools' list, the task tool that hands
/// each subagent its share of the same list, task_stop, and the work
/// plan's tools. The task and work tools post to `inbox`, which must be
/// the one the front-end empties.
pub(crate) fn tools(
    config: &Config,
    post_write: PostWrite,
    provider: Arc<dyn Provider>,
    subagents: Subagents,
    inbox: Inbox,
) -> Vec<Arc<dyn Tool>> {
    let mut tools = nth_tools::all(&config.tools, post_write);
    let stop = nth_session::TaskStop::new(subagents.clone());
    let work = nth_session::Work::new(
        provider.clone(),
        tools.clone(),
        subagents.clone(),
        inbox.clone(),
        config.work.limits(),
    );
    let task = nth_session::Task::new(
        provider,
        tools.clone(),
        subagents,
        inbox,
        config.task.limits(),
    );
    tools.push(Arc::new(task));
    tools.push(Arc::new(stop));
    tools.extend(work.tools());
    tools
}

/// Runs after every tool that writes a file. Its language servers are the
/// only ones this process starts: the read tool shares them.
pub(crate) fn post_write(config: &Config) -> PostWrite {
    PostWrite::new(
        Arc::new(nth_format::Formatters::new(&config.format)),
        nth_lsp::Lsp::new(&config.lsp),
    )
}

/// From the same servers the tools use, so the status bar shows what
/// checks the writes. The chat shuts them down when it ends.
pub(super) fn checkers(post_write: &PostWrite, config: &Config) -> nth_tui::Checkers {
    nth_tui::Checkers {
        lsp: post_write.lsp().clone(),
        formatters: post_write.formatters().clone(),
        rtk: config.tools.bash.rtk,
    }
}
