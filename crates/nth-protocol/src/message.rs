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

/// Arguments `ToolCall::summary` looks for, in order: the command itself
/// comes before the model's description of it.
const SUMMARY_KEYS: [&str; 9] = [
    "filePath",
    "command",
    "description",
    "name",
    "pattern",
    "query",
    "url",
    "panel",
    "item",
];

impl ToolCall {
    /// The one argument that best says what a call is doing, with paths
    /// shown relative to `cwd`.
    pub fn summary(&self, cwd: &Path) -> String {
        let Ok(args) = serde_json::from_str::<serde_json::Value>(&self.arguments) else {
            return self.arguments.clone();
        };
        // The question tool's headers, which name what it asked.
        if let Some(questions) = args["questions"].as_array() {
            let headers: Vec<&str> = questions
                .iter()
                .filter_map(|q| q["header"].as_str())
                .collect();
            return headers.join(", ");
        }
        if let Some(patch) = args["patchText"].as_str() {
            return patch_summary(patch, cwd);
        }
        let text = SUMMARY_KEYS
            .iter()
            .find_map(|key| args[key].as_str())
            .unwrap_or_default();
        let mut lines = text.trim().lines();
        let first = lines.next().unwrap_or_default();
        if lines.next().is_some() {
            return format!("{first} …");
        }
        let path = relative(text, cwd);
        match self.name.as_str() {
            "read" => format!("{path}{}", read_range(&args)),
            _ => path,
        }
    }
}

/// The lines a read call asked for, as `:start-end`, or `:start-` with no
/// limit; nothing when it reads from the top with the default limit.
fn read_range(args: &serde_json::Value) -> String {
    let offset = args["offset"].as_u64().filter(|&n| n > 0);
    // The read tool takes a limit of 0 as its default.
    let limit = args["limit"].as_u64().filter(|&n| n > 0);
    let start = offset.unwrap_or(1);
    match (offset, limit) {
        (None, None) => String::new(),
        (_, Some(limit)) => format!(":{start}-{}", start.saturating_add(limit - 1)),
        (Some(_), None) => format!(":{start}-"),
    }
}

/// The first file an apply_patch call touches, as a patch is too long to
/// show.
fn patch_summary(patch: &str, cwd: &Path) -> String {
    let mut paths = patch.lines().filter_map(|line| {
        ["*** Add File:", "*** Update File:", "*** Delete File:"]
            .iter()
            .find_map(|header| line.strip_prefix(header))
            .map(str::trim)
    });
    let first = relative(paths.next().unwrap_or_default(), cwd);
    match paths.next() {
        Some(_) => format!("{first} …"),
        None => first,
    }
}

fn relative(path: &str, cwd: &Path) -> String {
    // Matching whole components keeps `/repository` from being cut down
    // to `sitory` when cwd is `/repo`.
    match Path::new(path).strip_prefix(cwd) {
        Ok(rest) if rest.as_os_str().is_empty() => ".".to_string(),
        Ok(rest) => rest.to_string_lossy().into_owned(),
        Err(_) => path.to_string(),
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
    fn summary_shows_read_ranges() {
        let cwd = Path::new("/repo");
        let read = |args: &str| call(args).summary(cwd);

        assert_eq!(read(r#"{"filePath":"/repo/a.rs"}"#), "a.rs");
        assert_eq!(
            read(r#"{"filePath":"/repo/a.rs","offset":10,"limit":5}"#),
            "a.rs:10-14"
        );
        assert_eq!(read(r#"{"filePath":"/repo/a.rs","offset":10}"#), "a.rs:10-");
        assert_eq!(read(r#"{"filePath":"/repo/a.rs","limit":20}"#), "a.rs:1-20");
        assert_eq!(read(r#"{"filePath":"/repo/a.rs","limit":0}"#), "a.rs");
        let huge = format!(
            r#"{{"filePath":"/repo/a.rs","offset":2,"limit":{}}}"#,
            u64::MAX
        );
        assert_eq!(read(&huge), format!("a.rs:2-{}", u64::MAX));
        let write = ToolCall {
            name: "write".into(),
            ..call(r#"{"filePath":"/repo/a.rs","offset":10}"#)
        };
        assert_eq!(write.summary(cwd), "a.rs");
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
        assert_eq!(
            call(r#"{"pattern":"**/*.rs","path":"crates"}"#).summary(cwd),
            "**/*.rs"
        );
        assert_eq!(
            call(r#"{"query":"ratatui release"}"#).summary(cwd),
            "ratatui release"
        );
        assert_eq!(
            call(r#"{"url":"https://example.com","format":"text"}"#).summary(cwd),
            "https://example.com"
        );
        assert_eq!(
            call(r#"{"questions":[{"header":"Auth"},{"header":"Checks"}]}"#).summary(cwd),
            "Auth, Checks"
        );
        assert_eq!(call("not json").summary(cwd), "not json");
        assert_eq!(
            call(r#"{"patchText":"*** Begin Patch\n*** Update File: /repo/a.rs\n@@\n-a\n+b\n*** End Patch"}"#)
                .summary(cwd),
            "a.rs"
        );
        assert_eq!(
            call(r#"{"patchText":"*** Begin Patch\n*** Add File: a\n+x\n*** Delete File: b\n*** End Patch"}"#)
                .summary(cwd),
            "a …"
        );
    }
}
