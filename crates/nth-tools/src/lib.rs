mod bash;
mod read;

pub use bash::{Bash, BashConfig};
use nth_protocol::Tool;
pub use read::{Read, ReadConfig};
use serde::{Deserialize, Serialize};

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
        Box::new(Bash::new(config.bash.clone())),
    ]
}

fn parse_args<T: serde::de::DeserializeOwned>(args: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))
}
