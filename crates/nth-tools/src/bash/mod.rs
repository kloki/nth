mod rtk;

use std::{process::Stdio, time::Duration};

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{OutputSink, Tool, ToolContext, ToolResult, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Child,
};

use self::rtk::Rtk;
use crate::{
    command_output::{self, Cleaner},
    output::{self, tail},
    process::{self, DRAIN, KillGroupOnDrop, MIN_TIMEOUT_MS, kill_group},
};

/// Told to the model when rtk rewrites its commands, so it can follow the
/// pointers rtk leaves in what it cut.
const RTK_NOTE: &str = "\n# rtk\n- Common commands (git, cargo, test runners, linters and more) run through rtk, which compresses their output; the result then ends with the command that actually ran. Where rtk points to the full output with `rtk recall <id>`, run that to see it.\n";

/// How long before the session's tool ceiling a command is stopped: time
/// for the rtk rewrite (up to 2 s) that runs before bash's clock starts,
/// and to kill the command and hand over its output before the ceiling
/// drops the call.
const BEFORE_CEILING: Duration = Duration::from_secs(5);

/// How long a command may run.
#[derive(Debug, PartialEq)]
struct Limit {
    ms: u64,
    /// The session's tool ceiling set it, not the timeout asked for: a
    /// larger one would be cut the same.
    ceiling: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BashConfig {
    /// Used when the model gives no `timeout_ms`.
    pub default_timeout_ms: u64,
    /// Caps the `timeout_ms` the model asks for.
    pub max_timeout_ms: u64,
    /// Longer output keeps only its tail.
    pub max_output_chars: usize,
    /// Pass the model's commands through `rtk rewrite` when rtk is on PATH,
    /// to compress their output.
    pub rtk: bool,
}

impl Default for BashConfig {
    fn default() -> Self {
        Self {
            default_timeout_ms: 120_000,
            max_timeout_ms: 600_000,
            max_output_chars: output::MAX_CHARS,
            rtk: true,
        }
    }
}

#[derive(Default)]
pub struct Bash {
    config: BashConfig,
    /// Runs until the command exits or the call is dropped, whatever
    /// `timeout_ms` says.
    untimed: bool,
    rtk: Option<Rtk>,
}

impl Bash {
    pub fn new(config: BashConfig) -> Self {
        let rtk = config.rtk.then(Rtk::find).flatten();
        Self {
            config,
            untimed: false,
            rtk,
        }
    }

    /// How long a command may run: what the model asked for within the
    /// configured bounds, and over before the session's tool ceiling, so
    /// this clock stops the command and the model keeps what it printed.
    fn limit(&self, asked: Option<u64>, ceiling: Option<Duration>) -> Limit {
        // The configured default is the user's to set as low as they like;
        // the model's `0` would kill the command before it ran.
        let ms = asked
            .map(|t| t.max(MIN_TIMEOUT_MS))
            .unwrap_or(self.config.default_timeout_ms)
            .min(self.config.max_timeout_ms);
        let before = ceiling.map(|ceiling| {
            u64::try_from(ceiling.saturating_sub(BEFORE_CEILING).as_millis())
                .unwrap_or(u64::MAX)
                .max(MIN_TIMEOUT_MS)
        });
        match before {
            Some(before) if before < ms => Limit {
                ms: before,
                ceiling: true,
            },
            _ => Limit { ms, ceiling: false },
        }
    }

    /// For commands the user runs: they watch it and stop it themselves,
    /// so a long build is never cut off. Never through rtk: they read the
    /// output themselves.
    pub fn untimed(config: BashConfig) -> Self {
        Self {
            config,
            untimed: true,
            rtk: None,
        }
    }
}

#[derive(Deserialize)]
struct Args {
    command: String,
    timeout_ms: Option<u64>,
}

impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        let mut description = include_str!("description.txt")
            .replace(
                "{default_timeout_ms}",
                &self.config.default_timeout_ms.to_string(),
            )
            .replace("{max_timeout_ms}", &self.config.max_timeout_ms.to_string())
            .replace(
                "{max_output_chars}",
                &self.config.max_output_chars.to_string(),
            );
        if self.rtk.is_some() {
            description.push_str(RTK_NOTE);
        }
        ToolSpec {
            name: "bash",
            description,
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "The command to execute" },
                    "timeout_ms": { "type": "integer", "minimum": MIN_TIMEOUT_MS, "description": "Optional timeout in milliseconds" },
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
            crate::process::reject_old_timeout(&args)?;
            let args: Args = nth_protocol::parse_args(args)?;
            let limit = self.limit(args.timeout_ms, ctx.tool_timeout);
            let max_chars = self.config.max_output_chars;
            // `exec 2>&1` interleaves stderr into stdout in the order the
            // command wrote them, which two separate pipes cannot preserve.
            let rewritten = match &self.rtk {
                Some(rtk) => rtk.rewrite(&args.command, &ctx.cwd).await,
                None => None,
            };
            let command = rewritten.as_deref().unwrap_or(&args.command);
            let mut child = process::shell(&format!("exec 2>&1\n{command}"), &ctx.cwd, &ctx.session)
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| format!("failed to start bash: {e}"))?;
            let mut stdout = child.stdout.take().ok_or("bash stdout is not piped")?;
            // Declared after `child` so it drops first, while the child is
            // still unreaped and its pid cannot have been reused.
            let mut abandoned = KillGroupOnDrop(child.id());

            let mut buf = Vec::new();
            let mut streamed = Streamed::new(&ctx.output);
            let exit = wait_for_exit(&mut child, &mut stdout, &mut buf, &mut streamed);
            let run = if self.untimed {
                Ok(exit.await)
            } else {
                tokio::time::timeout(Duration::from_millis(limit.ms), exit).await
            };
            abandoned.0 = None;
            let status = match run {
                Ok(status) => status.map_err(|e| e.to_string())?,
                Err(_) => {
                    kill_group(&child);
                    streamed.rest(&buf).await;
                    let mut out = tail(&command_output::clean(&buf), max_chars);
                    let ms = limit.ms;
                    out.push_str(&match limit.ceiling {
                        true => format!(
                            "\n\ncommand terminated after {ms} ms, before the session's time limit for one tool call; a larger timeout would be cut the same. If it is expected to take longer, run it in the background with the monitor tool where you have it, or split it into shorter commands."
                        ),
                        false => format!(
                            "\n\ncommand terminated after exceeding timeout {ms} ms. If it is expected to take longer and is not waiting for input, retry with a larger timeout."
                        ),
                    });
                    return Err(out);
                }
            };
            // Processes the command put in the background may hold the pipe
            // open indefinitely; take what they already wrote and move on.
            let _ = tokio::time::timeout(DRAIN, stdout.read_to_end(&mut buf)).await;
            streamed.rest(&buf).await;

            let mut out = tail(&command_output::clean(&buf), max_chars);
            match status.code() {
                Some(0) => {}
                Some(code) => out.push_str(&format!("\n\n(exit code {code})")),
                None => out.push_str("\n\n(killed by signal)"),
            }
            if let Some(rewritten) = &rewritten {
                let note = format!("\n\n(ran as `{rewritten}`)");
                streamed.send(note.as_bytes()).await;
                out.push_str(&note);
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
    cleaner: Cleaner,
    /// How much of the output has been sent.
    sent: usize,
}

impl<'a> Streamed<'a> {
    fn new(sink: &'a OutputSink) -> Self {
        Self {
            sink,
            cleaner: Cleaner::default(),
            sent: 0,
        }
    }

    /// Sends the complete lines not sent yet. Awaiting a full channel pauses
    /// reading the command's output, which is the backpressure we want.
    async fn lines(&mut self, buf: &[u8]) {
        let Some(newline) = buf[self.sent..].iter().rposition(|&b| b == b'\n') else {
            return;
        };
        let end = self.sent + newline + 1;
        let clean = self.cleaner.push(&buf[self.sent..end]);
        self.send(&clean).await;
        self.sent = end;
    }

    /// Sends whatever is left, once the command is done.
    async fn rest(&mut self, buf: &[u8]) {
        if self.sent < buf.len() {
            let clean = self.cleaner.push(&buf[self.sent..]);
            self.send(&clean).await;
            self.sent = buf.len();
        }
    }

    async fn send(&self, bytes: &[u8]) {
        self.sink
            .send(String::from_utf8_lossy(bytes).into_owned())
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn bash(args: serde_json::Value) -> ToolResult {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        Bash::default().call(args, &ctx).await
    }

    /// A bash whose default timeout is `ms`: the one way to time out
    /// faster than the minimum the model may ask for.
    fn quick(ms: u64) -> Bash {
        Bash::new(BashConfig {
            default_timeout_ms: ms,
            rtk: false,
            ..BashConfig::default()
        })
    }

    const DEADLINE: Duration = Duration::from_secs(10);

    async fn next_text(rx: &mut tokio::sync::mpsc::Receiver<nth_protocol::Event>) -> String {
        match tokio::time::timeout(DEADLINE, rx.recv())
            .await
            .expect("output deadline")
        {
            Some(nth_protocol::Event::ToolOutput { text, .. }) => text,
            event => panic!("expected output, got {event:?}"),
        }
    }

    #[tokio::test]
    async fn streams_lines_before_the_command_exits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx) = tokio::sync::mpsc::channel(64);
        let ctx = ToolContext {
            output: OutputSink::new(tx, "1".into()),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        // The shell cannot finish until the test releases it through the FIFO.
        let bash = Bash::new(BashConfig {
            rtk: false,
            ..BashConfig::default()
        });
        let call = bash.call(json!({ "command": "mkfifo gate; echo one; read -r x < gate; echo two; printf three", "description": "t" }), &ctx);
        tokio::pin!(call);
        tokio::select! {
            text = next_text(&mut rx) => assert_eq!(text, "one\n"),
            result = &mut call => panic!("exited before release: {result:?}"),
        }
        let release = tokio::fs::write(dir.path().join("gate"), b"go\n");
        let (result, released) =
            tokio::time::timeout(DEADLINE, async { tokio::join!(&mut call, release) })
                .await
                .expect("completion deadline");
        released.expect("release");
        assert_eq!(result.expect("bash"), "one\ntwo\nthree");
        let mut rest = String::new();
        while let Ok(event) = rx.try_recv() {
            if let nth_protocol::Event::ToolOutput { text, .. } = event {
                rest.push_str(&text);
            }
        }
        assert_eq!(rest, "two\nthree");
    }

    #[tokio::test]
    async fn never_splits_a_character_across_sends() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let sink = OutputSink::new(tx, "1".into());
        let mut streamed = Streamed::new(&sink);
        streamed.lines(&[0xe2, 0x82]).await;
        assert!(rx.try_recv().is_err());
        streamed.lines(&[0xe2, 0x82, 0xac, b'\n']).await;
        assert_eq!(next_text(&mut rx).await, "€\n");
    }

    async fn assert_process_gone(pid: &str) {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let alive = std::process::Command::new("kill")
                    .args(["-0", pid.trim()])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .expect("run kill")
                    .success();
                if !alive || process_is_zombie(pid.trim()) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("background process survived cleanup");
    }

    #[cfg(target_os = "linux")]
    fn process_is_zombie(pid: &str) -> bool {
        let Ok(pid) = pid.parse::<u32>() else {
            return false;
        };
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')
                    .map(|(_, rest)| rest.trim().starts_with("Z "))
            })
            .unwrap_or(false)
    }

    #[cfg(not(target_os = "linux"))]
    fn process_is_zombie(_pid: &str) -> bool {
        false
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn running_process_is_not_a_zombie() {
        assert!(!process_is_zombie(&std::process::id().to_string()));
    }

    #[tokio::test]
    async fn interleaves_stdout_and_stderr() {
        let out =
            bash(json!({ "command": "echo a; echo b >&2; echo c", "description": "t" })).await;
        assert_eq!(out, Ok("a\nb\nc\n".to_string()));
    }

    #[tokio::test]
    async fn strips_escapes_from_results_and_streamed_output() {
        let dir = tempfile::tempdir().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let ctx = ToolContext {
            output: OutputSink::new(tx, "1".into()),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let out = Bash::default().call(json!({"command": "printf '\\033[31mred\\033[0m\\t€\\n'; printf '\\033]0;title\\007error\\n' >&2"}), &ctx).await.unwrap();
        assert_eq!(out, "red\t€\nerror\n");
        let mut streamed = String::new();
        while let Ok(nth_protocol::Event::ToolOutput { text, .. }) = rx.try_recv() {
            streamed.push_str(&text);
        }
        assert_eq!(streamed, out);
    }

    #[tokio::test]
    async fn reports_exit_code() {
        let out = bash(json!({ "command": "echo no; exit 3", "description": "t" })).await;
        assert_eq!(out, Ok("no\n\n\n(exit code 3)".to_string()));
    }

    #[tokio::test]
    async fn names_the_browser_session_after_the_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let args = json!({ "command": "echo \"[$AGENT_BROWSER_SESSION]\"", "description": "t" });
        let ctx = ToolContext {
            session: "abc".into(),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let out = Bash::default().call(args.clone(), &ctx).await;
        assert_eq!(out, Ok("[nth-abc]\n".to_string()));

        // Outside a session there is nothing to name it after.
        let empty = ToolContext {
            session: String::new(),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let out = Bash::default().call(args, &empty).await;
        let inherited = std::env::var("AGENT_BROWSER_SESSION").unwrap_or_default();
        assert_eq!(out, Ok(format!("[{inherited}]\n")));
    }

    #[tokio::test]
    async fn uses_configured_limits() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let bash = Bash::new(BashConfig {
            default_timeout_ms: 50,
            max_output_chars: 3,
            rtk: false,
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
            Ok("...output truncated, showing the last 3 of 6 characters...\ndef".to_string())
        );
        let out = bash
            .call(json!({ "command": "sleep 5", "description": "t" }), &ctx)
            .await;
        assert!(out.is_err_and(|e| e.contains("timeout 50 ms")));
    }

    #[test]
    fn the_timeout_ends_before_the_tool_ceiling() {
        let bash = Bash::new(BashConfig {
            max_timeout_ms: 3_600_000,
            rtk: false,
            ..BashConfig::default()
        });
        let ceiling = Some(Duration::from_secs(900));

        let limit = |ms, ceiling| Limit { ms, ceiling };

        assert_eq!(bash.limit(Some(1_800_000), ceiling), limit(895_000, true));
        assert_eq!(bash.limit(Some(60_000), ceiling), limit(60_000, false));
        assert_eq!(bash.limit(Some(1_800_000), None), limit(1_800_000, false));
        // Never under the minimum, however low the ceiling.
        let low = Some(Duration::from_secs(2));
        assert_eq!(bash.limit(Some(60_000), low), limit(MIN_TIMEOUT_MS, true));
    }

    #[tokio::test]
    async fn a_command_past_the_tool_ceiling_times_out_with_its_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext {
            tool_timeout: Some(BEFORE_CEILING + Duration::from_secs(1)),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let bash = Bash::new(BashConfig {
            rtk: false,
            ..BashConfig::default()
        });

        let out = bash
            .call(
                json!({ "command": "echo early; sleep 30", "timeout_ms": 60_000, "description": "t" }),
                &ctx,
            )
            .await;

        let out = out.expect_err("the command was stopped");
        assert!(out.starts_with("early\n"), "{out}");
        assert!(out.contains("terminated after 1000 ms"), "{out}");
        assert!(out.contains("the monitor tool"), "{out}");
        assert!(!out.contains("retry with a larger timeout"), "{out}");
    }

    #[tokio::test]
    async fn runs_what_rtk_rewrites_the_command_to() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let bash = Bash {
            rtk: Some(rtk::tests::fake(dir.path(), "echo 'echo b'")),
            ..Bash::default()
        };
        assert!(bash.spec().description.contains("rtk recall"));
        let out = bash
            .call(json!({ "command": "echo a", "description": "t" }), &ctx)
            .await;
        assert_eq!(out, Ok("b\n\n\n(ran as `echo b`)".to_string()));
    }

    #[test]
    fn rtk_is_off_when_the_config_says_so_and_for_the_user() {
        let off = Bash::new(BashConfig {
            rtk: false,
            ..BashConfig::default()
        });
        assert!(off.rtk.is_none());
        assert!(!off.spec().description.contains("rtk"));
        assert!(Bash::untimed(BashConfig::default()).rtk.is_none());
    }

    #[tokio::test]
    async fn untimed_runs_past_the_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let bash = Bash::untimed(BashConfig {
            default_timeout_ms: 50,
            max_timeout_ms: 50,
            ..BashConfig::default()
        });
        let out = bash
            .call(json!({ "command": "sleep 0.3; echo done" }), &ctx)
            .await;
        assert_eq!(out, Ok("done\n".to_string()));
    }

    #[tokio::test]
    async fn a_too_short_timeout_is_raised_to_the_minimum() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let out = quick(50)
            .call(
                json!({ "command": "sleep 0.2; echo hi", "timeout_ms": 0, "description": "t" }),
                &ctx,
            )
            .await;
        assert_eq!(out, Ok("hi\n".to_string()));
    }

    #[tokio::test]
    async fn timeout_keeps_output_so_far() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let out = quick(200)
            .call(
                json!({ "command": "echo early; sleep 5", "description": "t" }),
                &ctx,
            )
            .await;
        assert!(out.is_err_and(|e| e.starts_with("early\n") && e.contains("timeout 200 ms")));
    }

    #[tokio::test]
    async fn background_process_does_not_block() {
        let started = std::time::Instant::now();
        let out = bash(
            json!({ "command": "sleep 30 & echo hi", "timeout_ms": 10_000, "description": "t" }),
        )
        .await;
        assert_eq!(out, Ok("hi\n".to_string()));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn timeout_kills_background_processes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let out = quick(2_000)
            .call(
                json!({ "command": "sleep 30 & echo $! > pid; sleep 30", "description": "t" }),
                &ctx,
            )
            .await;
        assert!(out.is_err());
        let pid = std::fs::read_to_string(dir.path().join("pid")).expect("pid file");
        assert_process_gone(&pid).await;
    }

    #[tokio::test]
    async fn dropping_a_running_call_kills_its_processes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let ctx = ToolContext {
            output: OutputSink::new(tx, "1".into()),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let bash = Bash::new(BashConfig {
            rtk: false,
            ..BashConfig::default()
        });
        {
            let call = bash.call(
                json!({ "command": "sleep 30 & echo $! > pid; echo ready; wait", "description": "t" }),
                &ctx,
            );
            tokio::pin!(call);
            tokio::select! {
                text = next_text(&mut rx) => assert_eq!(text, "ready\n"),
                result = &mut call => panic!("exited before cancellation: {result:?}"),
            }
        }
        let pid = std::fs::read_to_string(dir.path().join("pid")).expect("pid file");
        assert_process_gone(&pid).await;
    }
}
