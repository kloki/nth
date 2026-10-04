mod apply_patch;
mod bash;
mod edit;
mod glob;
mod grep;
mod read;
mod skill;
mod write;

pub use apply_patch::ApplyPatch;
pub use bash::{Bash, BashConfig};
pub use edit::Edit;
pub use glob::Glob;
pub use grep::Grep;
use nth_protocol::Tool;
pub use read::{Read, ReadConfig};
use serde::{Deserialize, Serialize};
pub use skill::Skill;
pub use write::Write;

#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    pub read: ReadConfig,
    pub bash: BashConfig,
}

/// Every tool nth ships. Agents filter this list by name.
pub fn all(config: &ToolsConfig) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(Read::new(config.read.clone())),
        Box::new(Write),
        Box::new(Edit),
        Box::new(ApplyPatch),
        Box::new(Bash::new(config.bash.clone())),
        Box::new(Glob),
        Box::new(Grep),
        Box::new(Skill),
    ]
}

fn parse_args<T: serde::de::DeserializeOwned>(args: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))
}
