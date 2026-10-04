mod bash;
mod read;
mod skill;
mod write;

pub use bash::{Bash, BashConfig};
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
        Box::new(Bash::new(config.bash.clone())),
        Box::new(Skill),
    ]
}

fn parse_args<T: serde::de::DeserializeOwned>(args: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))
}
