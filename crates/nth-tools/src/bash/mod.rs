use std::{process::Stdio, time::Duration};

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT_CHARS: usize = 30_000;

pub struct Bash;

#[derive(Deserialize)]
struct Args {
    command: String,
    timeout: Option<u64>,
}

impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash",
            description: include_str!("description.txt"),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command to execute" },
                    "timeout": { "type": "integer", "minimum": 1, "description": "Optional timeout in milliseconds" },
                    "description": { "type": "string", "description": "Clear, concise description of what this command does in 5-10 words" }
                },
                "required": ["command", "description"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            let args: Args = crate::parse_args(args)?;
            let timeout_ms = args.timeout.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS);
            // `exec 2>&1` interleaves stderr into stdout in the order the
            // command wrote them, which two separate pipes cannot preserve.
            let child = tokio::process::Command::new("bash")
                .arg("-c")
                .arg(format!("exec 2>&1\n{}", args.command))
                .current_dir(&ctx.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| format!("failed to start bash: {e}"))?;

            let output = tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
                .await
                .map_err(|_| {
                    format!(
                        "command terminated after exceeding timeout {timeout_ms} ms. If it is expected to take longer and is not waiting for input, retry with a larger timeout."
                    )
                })?
                .map_err(|e| e.to_string())?;

            let mut out = tail(&String::from_utf8_lossy(&output.stdout));
            match output.status.code() {
                Some(0) => {}
                Some(code) => out.push_str(&format!("\n\n(exit code {code})")),
                None => out.push_str("\n\n(killed by signal)"),
            }
            Ok(out)
        }
        .boxed()
    }
}

fn tail(text: &str) -> String {
    let count = text.chars().count();
    if count <= MAX_OUTPUT_CHARS {
        return text.to_string();
    }
    let kept: String = text.chars().skip(count - MAX_OUTPUT_CHARS).collect();
    format!("...output truncated, showing the last {MAX_OUTPUT_CHARS} characters...\n{kept}")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn bash(args: serde_json::Value) -> ToolResult {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext {
            cwd: dir.path().to_path_buf(),
        };
        Bash.call(args, &ctx).await
    }

    #[tokio::test]
    async fn interleaves_stdout_and_stderr() {
        let out =
            bash(json!({ "command": "echo a; echo b >&2; echo c", "description": "t" })).await;
        assert_eq!(out, Ok("a\nb\nc\n".to_string()));
    }

    #[tokio::test]
    async fn reports_exit_code() {
        let out = bash(json!({ "command": "echo no; exit 3", "description": "t" })).await;
        assert_eq!(out, Ok("no\n\n\n(exit code 3)".to_string()));
    }

    #[tokio::test]
    async fn times_out() {
        let out = bash(json!({ "command": "sleep 5", "timeout": 50, "description": "t" })).await;
        assert!(out.is_err_and(|e| e.contains("timeout 50 ms")));
    }
}
