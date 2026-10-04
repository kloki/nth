use std::{process::Stdio, time::Duration};

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{OutputSink, Tool, ToolContext, ToolResult, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Child,
};

use crate::process::{self, KillGroupOnDrop, kill_group};

/// How long to keep reading after bash exits, for output still in the pipe.
const DRAIN: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BashConfig {
    /// Used when the model gives no `timeout`.
    pub default_timeout_ms: u64,
    /// Caps the `timeout` the model asks for.
    pub max_timeout_ms: u64,
    /// Longer output keeps only its tail.
    pub max_output_chars: usize,
}

impl Default for BashConfig {
    fn default() -> Self {
        Self {
            default_timeout_ms: 120_000,
            max_timeout_ms: 600_000,
            max_output_chars: 30_000,
        }
    }
}

#[derive(Default)]
pub struct Bash {
    config: BashConfig,
}

impl Bash {
    pub fn new(config: BashConfig) -> Self {
        Self { config }
    }
}

#[derive(Deserialize)]
struct Args {
    command: String,
    timeout: Option<u64>,
}

impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash",
            description: include_str!("description.txt")
                .replace(
                    "{default_timeout_ms}",
                    &self.config.default_timeout_ms.to_string(),
                )
                .replace("{max_timeout_ms}", &self.config.max_timeout_ms.to_string())
                .replace(
                    "{max_output_chars}",
                    &self.config.max_output_chars.to_string(),
                ),
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
            let timeout_ms = args
                .timeout
                .unwrap_or(self.config.default_timeout_ms)
                .min(self.config.max_timeout_ms);
            let max_chars = self.config.max_output_chars;
            // `exec 2>&1` interleaves stderr into stdout in the order the
            // command wrote them, which two separate pipes cannot preserve.
            let mut child = process::shell(&format!("exec 2>&1\n{}", args.command), &ctx.cwd)
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| format!("failed to start bash: {e}"))?;
            let mut stdout = child.stdout.take().ok_or("bash stdout is not piped")?;
            // Declared after `child` so it drops first, while the child is
            // still unreaped and its pid cannot have been reused.
            let mut abandoned = KillGroupOnDrop(child.id());

            let mut buf = Vec::new();
            let mut streamed = Streamed::new(&ctx.output);
            let run = tokio::time::timeout(
                Duration::from_millis(timeout_ms),
                wait_for_exit(&mut child, &mut stdout, &mut buf, &mut streamed),
            )
            .await;
            abandoned.0 = None;
            let status = match run {
                Ok(status) => status.map_err(|e| e.to_string())?,
                Err(_) => {
                    kill_group(&child);
                    streamed.rest(&buf).await;
                    let mut out = tail(&String::from_utf8_lossy(&buf), max_chars);
                    out.push_str(&format!(
                        "\n\ncommand terminated after exceeding timeout {timeout_ms} ms. If it is expected to take longer and is not waiting for input, retry with a larger timeout."
                    ));
                    return Err(out);
                }
            };
            // Processes the command put in the background may hold the pipe
            // open indefinitely; take what they already wrote and move on.
            let _ = tokio::time::timeout(DRAIN, stdout.read_to_end(&mut buf)).await;
            streamed.rest(&buf).await;

            let mut out = tail(&String::from_utf8_lossy(&buf), max_chars);
            match status.code() {
                Some(0) => {}
                Some(code) => out.push_str(&format!("\n\n(exit code {code})")),
                None => out.push_str("\n\n(killed by signal)"),
            }
            Ok(out)
        }
        .boxed()
    }
}

/// Waits for bash itself to exit rather than for stdout to close, which
/// background processes can delay forever.
async fn wait_for_exit(
    child: &mut Child,
    stdout: &mut (impl AsyncRead + Unpin),
    buf: &mut Vec<u8>,
    streamed: &mut Streamed<'_>,
) -> std::io::Result<std::process::ExitStatus> {
    loop {
        tokio::select! {
            read = stdout.read_buf(buf) => {
                if read? == 0 {
                    return child.wait().await;
                }
                streamed.lines(buf).await;
            }
            status = child.wait() => return status,
        }
    }
}

/// Streams the output as it arrives, in whole lines, so a multi-byte
/// character split across two reads is never sent in halves.
struct Streamed<'a> {
    sink: &'a OutputSink,
    /// How much of the output has been sent.
    sent: usize,
}

impl<'a> Streamed<'a> {
    fn new(sink: &'a OutputSink) -> Self {
        Self { sink, sent: 0 }
    }

    /// Sends the complete lines not sent yet. Awaiting a full channel pauses
    /// reading the command's output, which is the backpressure we want.
    async fn lines(&mut self, buf: &[u8]) {
        let Some(newline) = buf[self.sent..].iter().rposition(|&b| b == b'\n') else {
            return;
        };
        let end = self.sent + newline + 1;
        self.send(&buf[self.sent..end]).await;
        self.sent = end;
    }

    /// Sends whatever is left, once the command is done.
    async fn rest(&mut self, buf: &[u8]) {
        if self.sent < buf.len() {
            self.send(&buf[self.sent..]).await;
            self.sent = buf.len();
        }
    }

    async fn send(&self, bytes: &[u8]) {
        self.sink
            .send(String::from_utf8_lossy(bytes).into_owned())
            .await;
    }
}

fn tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().skip(count - max_chars).collect();
    format!("...output truncated, showing the last {max_chars} characters...\n{kept}")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn bash(args: serde_json::Value) -> ToolResult {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        Bash::default().call(args, &ctx).await
    }

    /// Runs `command` with a sink, and returns what it streamed.
    async fn streamed(command: &str) -> Vec<String> {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext {
            output: OutputSink::new(tx, "1".into()),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let out = Bash::default()
            .call(json!({ "command": command, "description": "t" }), &ctx)
            .await;
        assert!(out.is_ok());
        let mut texts = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let nth_protocol::Event::ToolOutput { text, .. } = event {
                texts.push(text);
            }
        }
        texts
    }

    #[tokio::test]
    async fn streams_lines_before_the_command_exits() {
        let texts = streamed("echo one; sleep 0.2; echo two; printf three").await;
        assert_eq!(texts, ["one\n", "two\n", "three"]);
    }

    #[tokio::test]
    async fn never_splits_a_character_across_sends() {
        // The euro sign is three bytes; write them in two separate reads.
        let texts = streamed(r"printf '\xe2\x82'; sleep 0.1; printf '\xac\n'").await;
        assert_eq!(texts.concat(), "€\n");
        assert!(texts.iter().all(|t| !t.contains('\u{fffd}')));
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
    async fn uses_configured_limits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let bash = Bash::new(BashConfig {
            default_timeout_ms: 50,
            max_output_chars: 3,
            ..BashConfig::default()
        });
        assert!(bash.spec().description.contains("time out after 50ms"));
        let out = bash
            .call(
                json!({ "command": "printf abcdef", "description": "t" }),
                &ctx,
            )
            .await;
        assert_eq!(
            out,
            Ok("...output truncated, showing the last 3 characters...\ndef".to_string())
        );
        let out = bash
            .call(json!({ "command": "sleep 5", "description": "t" }), &ctx)
            .await;
        assert!(out.is_err_and(|e| e.contains("timeout 50 ms")));
    }

    #[tokio::test]
    async fn times_out() {
        let out = bash(json!({ "command": "sleep 5", "timeout": 50, "description": "t" })).await;
        assert!(out.is_err_and(|e| e.contains("timeout 50 ms")));
    }

    #[tokio::test]
    async fn timeout_keeps_output_so_far() {
        let out =
            bash(json!({ "command": "echo early; sleep 5", "timeout": 200, "description": "t" }))
                .await;
        assert!(out.is_err_and(|e| e.starts_with("early\n") && e.contains("timeout 200 ms")));
    }

    #[tokio::test]
    async fn background_process_does_not_block() {
        let started = std::time::Instant::now();
        let out =
            bash(json!({ "command": "sleep 30 & echo hi", "timeout": 10_000, "description": "t" }))
                .await;
        assert_eq!(out, Ok("hi\n".to_string()));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn timeout_kills_background_processes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let out = Bash::default()
            .call(
                json!({ "command": "sleep 30 & echo $! > pid; sleep 30", "timeout": 200, "description": "t" }),
                &ctx,
            )
            .await;
        assert!(out.is_err());
        let pid = std::fs::read_to_string(dir.path().join("pid")).expect("pid file");
        // Give the kernel a moment to deliver SIGKILL and let init reap it.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .status()
            .expect("run kill")
            .success();
        assert!(
            !alive,
            "background sleep {} survived the timeout",
            pid.trim()
        );
    }

    #[tokio::test]
    async fn dropping_a_running_call_kills_its_processes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let bash = Bash::default();
        let call = bash.call(
            json!({ "command": "sleep 30 & echo $! > pid; sleep 30", "description": "t" }),
            &ctx,
        );
        let out = tokio::time::timeout(Duration::from_millis(300), call).await;
        assert!(out.is_err(), "the call should still be running");
        let pid = std::fs::read_to_string(dir.path().join("pid")).expect("pid file");
        tokio::time::sleep(Duration::from_millis(200)).await;
        let alive = std::process::Command::new("kill")
            .args(["-0", pid.trim()])
            .status()
            .expect("run kill")
            .success();
        assert!(!alive, "background sleep {} survived the drop", pid.trim());
    }
}
