mod bash;
mod read;
mod write;

pub use bash::Bash;
use nth_protocol::Tool;
pub use read::Read;
pub use write::Write;

/// Every tool nth ships. Agents filter this list by name.
pub fn all() -> Vec<Box<dyn Tool>> {
    vec![Box::new(Read), Box::new(Write), Box::new(Bash)]
}

fn parse_args<T: serde::de::DeserializeOwned>(args: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid arguments: {e}"))
}
