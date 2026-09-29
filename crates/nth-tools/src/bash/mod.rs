use std::{process::Stdio, time::Duration};

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{Tool, ToolContext, ToolResult, ToolSpec};
use serde::Deserialize;
use serde_json::json;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Child,
};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_OUTPUT_CHARS: usize = 30_000;
/// How long to keep reading after bash exits, for output still in the pipe.
const DRAIN: Duration = Duration::from_millis(100);

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
            let mut child = tokio::process::Command::new("bash")
                .arg("-c")
                .arg(format!("exec 2>&1\n{}", args.command))
                .current_dir(&ctx.cwd)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                // Own process group, so a timeout can kill everything the
                // command started, not just bash.
                .process_group(0)
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| format!("failed to start bash: {e}"))?;
            let mut stdout = child.stdout.take().ok_or("bash stdout is not piped")?;
            // Declared after `child` so it drops first, while the child is
            // still unreaped and its pid cannot have been reused.
            let mut abandoned = KillGroupOnDrop(child.id());

            let mut buf = Vec::new();
            let run = tokio::time::timeout(
                Duration::from_millis(timeout_ms),
                wait_for_exit(&mut child, &mut stdout, &mut buf),
            )
            .await;
            abandoned.0 = None;
            let status = match run {
                Ok(status) => status.map_err(|e| e.to_string())?,
                Err(_) => {
                    kill_group(&child);
                    let mut out = tail(&String::from_utf8_lossy(&buf));
                    out.push_str(&format!(
                        "\n\ncommand terminated after exceeding timeout {timeout_ms} ms. If it is expected to take longer and is not waiting for input, retry with a larger timeout."
                    ));
                    return Err(out);
                }
            };
            // Processes the command put in the background may hold the pipe
            // open indefinitely; take what they already wrote and move on.
            let _ = tokio::time::timeout(DRAIN, stdout.read_to_end(&mut buf)).await;

            let mut out = tail(&String::from_utf8_lossy(&buf));
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
) -> std::io::Result<std::process::ExitStatus> {
    loop {
        tokio::select! {
            read = stdout.read_buf(buf) => {
                if read? == 0 {
                    return child.wait().await;
                }
            }
            status = child.wait() => return status,
        }
    }
}

/// Kills the command's whole group if the call is dropped mid-run, as when
/// a front-end quits during a turn. `kill_on_drop` alone reaches only bash,
/// not what it started.
struct KillGroupOnDrop(Option<u32>);

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_pid_group(pid);
        }
    }
}

fn kill_group(child: &Child) {
    if let Some(pid) = child.id() {
        kill_pid_group(pid);
    }
}

fn kill_pid_group(pid: u32) {
    let Ok(pgid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: kill has no memory-safety preconditions. The group id is the
    // pid of our own child, which is still unreaped and so cannot be reused.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
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
        let ctx = ToolContext {
            cwd: dir.path().to_path_buf(),
        };
        let out = Bash
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
        let ctx = ToolContext {
            cwd: dir.path().to_path_buf(),
        };
        let call = Bash.call(
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
