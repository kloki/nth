use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Message {
    System(String),
    User(String),
    Assistant(AssistantMessage),
    ToolResult { call_id: String, content: String },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AssistantMessage {
    pub text: String,
    /// Some models (Kimi, DeepSeek) require their reasoning to be sent back
    /// on later requests, so it is kept rather than dropped after display.
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON as the model produced it; parsed only when the tool runs, so
    /// a malformed call becomes a tool error the model can correct.
    pub arguments: String,
}

impl ToolCall {
    /// The one argument that best says what a call is doing, with paths
    /// shown relative to `cwd`.
    pub fn summary(&self, cwd: &Path) -> String {
        let Ok(args) = serde_json::from_str::<serde_json::Value>(&self.arguments) else {
            return self.arguments.clone();
        };
        // The command itself over the model's description of it.
        let text = ["filePath", "command", "description", "name"]
            .iter()
            .find_map(|key| args[key].as_str())
            .unwrap_or_default();
        let mut lines = text.trim().lines();
        let first = lines.next().unwrap_or_default();
        if lines.next().is_some() {
            return format!("{first} …");
        }
        // Matching whole components keeps `/repository` from being cut down
        // to `sitory` when cwd is `/repo`.
        match Path::new(text).strip_prefix(cwd) {
            Ok(rest) if rest.as_os_str().is_empty() => ".".to_string(),
            Ok(rest) => rest.to_string_lossy().into_owned(),
            Err(_) => text.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(arguments: &str) -> ToolCall {
        ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: arguments.into(),
        }
    }

    #[test]
    fn summary_shows_paths_relative_to_cwd() {
        let cwd = Path::new("/repo");

        assert_eq!(
            call(r#"{"filePath":"/repo/src/a.rs"}"#).summary(cwd),
            "src/a.rs"
        );
        assert_eq!(call(r#"{"filePath":"/repo"}"#).summary(cwd), ".");
        assert_eq!(
            call(r#"{"filePath":"/repository/src/x.rs"}"#).summary(cwd),
            "/repository/src/x.rs"
        );
        assert_eq!(call(r#"{"command":"ls"}"#).summary(cwd), "ls");
        assert_eq!(
            call(r#"{"command":"cargo test","description":"Run tests"}"#).summary(cwd),
            "cargo test"
        );
        assert_eq!(
            call(r#"{"command":"cd crates\ncargo test\n"}"#).summary(cwd),
            "cd crates …"
        );
        assert_eq!(
            call(r#"{"name":"research-opencode"}"#).summary(cwd),
            "research-opencode"
        );
        assert_eq!(call("not json").summary(cwd), "not json");
    }
}
