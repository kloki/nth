mod apply_patch;
mod bash;
mod blocking;
mod bom;
mod command_output;
mod context_preview;
mod curios;
mod edit;
mod glob;
mod grep;
mod monitor;
mod output;
mod panel;
mod path;
mod post_write;
mod process;
mod question;
mod read;
mod schedule_wakeup;
mod show_user;
mod skill;
mod webfetch;
mod websearch;
mod worktree;
mod write;

use std::sync::Arc;

pub use apply_patch::ApplyPatch;
pub use bash::{Bash, BashConfig};
pub use context_preview::ContextPreview;
pub use curios::{CurioPublish, CurioRead, CurioReply};
pub use edit::Edit;
pub use glob::Glob;
pub use grep::Grep;
pub use monitor::{Monitor, MonitorStop};
use nth_protocol::Tool;
pub use panel::Panel;
pub use post_write::PostWrite;
pub use question::Question;
pub use read::{Read, ReadConfig};
pub use schedule_wakeup::ScheduleWakeup;
use serde::{Deserialize, Serialize};
pub use show_user::ShowUser;
pub use skill::Skill;
pub use webfetch::WebFetch;
pub use websearch::{Websearch, WebsearchConfig};
pub use worktree::{EnterWorktree, ExitWorktree};
pub use write::Write;

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    pub read: ReadConfig,
    pub bash: BashConfig,
    pub websearch: WebsearchConfig,
}

/// Every tool nth ships. Agents filter this list by name. `post_write`
/// runs after every tool that writes a file. Reads share its language
/// servers, so they warm the ones a later write asks.
pub fn all(config: &ToolsConfig, post_write: PostWrite) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(Read::new(config.read.clone(), post_write.lsp().clone())),
        Arc::new(Write::new(post_write.clone())),
        Arc::new(Edit::new(post_write.clone())),
        Arc::new(ApplyPatch::new(post_write)),
        Arc::new(Bash::new(config.bash.clone())),
        Arc::new(Monitor),
        Arc::new(MonitorStop),
        Arc::new(Glob),
        Arc::new(Grep),
        Arc::new(Skill),
        Arc::new(WebFetch),
        Arc::new(Websearch::new(config.websearch.clone())),
        Arc::new(Question),
        Arc::new(Panel),
        Arc::new(ContextPreview),
        Arc::new(ScheduleWakeup),
        Arc::new(ShowUser),
        Arc::new(CurioPublish),
        Arc::new(CurioRead),
        Arc::new(CurioReply),
        Arc::new(EnterWorktree),
        Arc::new(ExitWorktree),
    ]
}
