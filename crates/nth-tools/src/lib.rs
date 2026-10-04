mod apply_patch;
mod bash;
mod bom;
mod edit;
mod glob;
mod grep;
mod monitor;
mod panel;
mod post_write;
mod process;
mod question;
mod read;
mod skill;
mod webfetch;
mod websearch;
mod write;

pub use apply_patch::ApplyPatch;
pub use bash::{Bash, BashConfig};
pub use edit::Edit;
pub use glob::Glob;
pub use grep::Grep;
pub use monitor::{Monitor, MonitorStop};
use nth_protocol::Tool;
pub use panel::Panel;
pub use post_write::PostWrite;
pub use question::Question;
pub use read::{Read, ReadConfig};
use serde::{Deserialize, Serialize};
pub use skill::Skill;
pub use webfetch::WebFetch;
pub use websearch::{Websearch, WebsearchConfig};
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
pub fn all(config: &ToolsConfig, post_write: PostWrite) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(Read::new(config.read.clone(), post_write.lsp().clone())),
        Box::new(Write::new(post_write.clone())),
        Box::new(Edit::new(post_write.clone())),
        Box::new(ApplyPatch::new(post_write)),
        Box::new(Bash::new(config.bash.clone())),
        Box::new(Monitor),
        Box::new(MonitorStop),
        Box::new(Glob),
        Box::new(Grep),
        Box::new(Skill),
        Box::new(WebFetch),
        Box::new(Websearch::new(config.websearch.clone())),
        Box::new(Question),
        Box::new(Panel),
    ]
}

fn parse_args<T: serde::de::DeserializeOwned>(args: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))
}
