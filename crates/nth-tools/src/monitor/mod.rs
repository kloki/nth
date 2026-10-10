use std::{
    collections::VecDeque,
    path::Path,
    pin::Pin,
    process::Stdio,
    time::{Duration, Instant},
};

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{
    MonitorEnd, MonitorEvent, MonitorId, Monitors, Registered, StoppedBy, Stream, Tool,
    ToolContext, ToolResult, ToolSpec,
};
use serde::Deserialize;
use serde_json::json;
use tokio::{
    fs::File,
    io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Child,
};

use crate::process::{self, KillGroupOnDrop, MIN_TIMEOUT_MS, kill_group};

const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const MAX_TIMEOUT_MS: u64 = 1_800_000;

/// More stdout lines than this within [`FLOOD_WINDOW`] stop the monitor:
/// the model could not take them in.
const FLOOD_LINES: usize = 100;
const FLOOD_WINDOW: Duration = Duration::from_secs(10);

/// How long to keep reading after the command exits, for output still in
/// its pipes.
const DRAIN: Duration = Duration::from_millis(100);

/// Leaves a command running in the background and hands its stdout lines to
/// the model as they come.
pub struct Monitor;

#[derive(Deserialize)]
struct Args {
    command: String,
    description: String,
    timeout_ms: Option<u64>,
}

impl Tool for Monitor {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "monitor",
            description: include_str!("description.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string", "description": "Shell command or script. Each stdout line is an event; exit ends the watch." },
                    "description": { "type": "string", "description": "Short description of what you are monitoring, shown in every notice." },
                    "timeout_ms": { "type": "integer", "minimum": MIN_TIMEOUT_MS, "maximum": MAX_TIMEOUT_MS, "description": "Kill the monitor after this deadline. Default 300000." }
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
            if !ctx.monitors.reaches_front_end() {
                return Err("there is no front-end to deliver monitor events to in this session; use bash and wait for the command instead".into());
            }
            let timeout = Duration::from_millis(
                args.timeout_ms
                    .unwrap_or(DEFAULT_TIMEOUT_MS)
                    .clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS),
            );
            let child = process::shell(&args.command, &ctx.cwd)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| format!("failed to start bash: {e}"))?;
            // Armed before anything can fail: a command whose watch never
            // starts is killed with everything it started, where dropping
            // the child alone would reach only bash.
            let abandoned = KillGroupOnDrop(child.id());
            let registered = ctx
                .monitors
                .register(&args.description, &args.command)
                .await
                .ok_or("the front-end is gone")?;
            let id = registered.id;
            let (log, log_note) = match open_log(&registered.log).await {
                Ok(file) => (Some(file), format!("The full output, stderr included, is in {}.", registered.log.display())),
                Err(e) => (None, format!("Its output is not logged: {e}.")),
            };
            tokio::spawn(watch(
                ctx.monitors.clone(),
                registered,
                child,
                abandoned,
                log,
                timeout,
            ));
            Ok(format!(
                "Started monitor {id} (\"{}\"), for at most {}s. Each stdout line reaches you as a notice. {log_note} Stop it with monitor_stop.",
                args.description,
                timeout.as_secs()
            ))
        }
        .boxed()
    }
}

async fn open_log(path: &Path) -> std::io::Result<File> {
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    File::create(path).await
}

/// Runs until the command exits, the timeout, a stop, a flood or the
/// front-end leaving, and reports how it ended. Everything the command
/// started dies with it unless it exited by itself: then, as with bash,
/// what it put in the background lives on.
async fn watch(
    monitors: Monitors,
    registered: Registered,
    mut child: Child,
    mut abandoned: KillGroupOnDrop,
    log: Option<File>,
    timeout: Duration,
) {
    let Registered { id, stop, .. } = registered;
    let mut out = child.stdout.take().map(Pipe::new);
    let mut err = child.stderr.take().map(Pipe::new);
    let mut watch = Watch {
        monitors: &monitors,
        id,
        log,
        started: Instant::now(),
        events: 0,
        recent: VecDeque::new(),
    };
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);

    let end = loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break MonitorEnd::Stopped(monitors.stopped_by(id)),
            _ = &mut deadline => break MonitorEnd::TimedOut { after_ms: timeout.as_millis() as u64 },
            line = next_line(&mut out) => match line {
                Some(line) => {
                    if let Some(end) = watch.line(line, Stream::Stdout).await {
                        break end;
                    }
                }
                None => out = None,
            },
            line = next_line(&mut err) => match line {
                Some(line) => {
                    if let Some(end) = watch.line(line, Stream::Stderr).await {
                        break end;
                    }
                }
                None => err = None,
            },
            // Waits for bash rather than for its pipes to close, which
            // processes it put in the background can delay forever.
            status = child.wait() => {
                let status = status.ok().and_then(|s| s.code());
                let drained = tokio::time::timeout(DRAIN, async {
                    while let Some(line) = next_line(&mut out).await {
                        watch.line(line, Stream::Stdout).await;
                    }
                    while let Some(line) = next_line(&mut err).await {
                        watch.line(line, Stream::Stderr).await;
                    }
                });
                let _ = drained.await;
                // Reaped, so the group is no longer ours to kill: its pid
                // may be reused, and what is left in it was meant to
                // outlive the command.
                abandoned.0 = None;
                break MonitorEnd::Exited(status);
            }
        }
    };
    if abandoned.0.is_some() {
        // Stopped, timed out or flooded: the command and all it started go.
        kill_group(&child);
        abandoned.0 = None;
    }
    let events = watch.events;
    watch.write_log(&format!("{end}")).await;
    watch.flush_log().await;
    monitors
        .event(MonitorEvent::Ended { id, end, events })
        .await;
}

/// One of the command's pipes with the start of a line that has not ended
/// yet. The line is kept here rather than in the reading future, which the
/// `select!` drops whenever another arm wins: a half-read line must survive
/// that, or stdout loses bytes every time stderr speaks first.
struct Pipe<R> {
    reader: BufReader<R>,
    partial: Vec<u8>,
}

impl<R: tokio::io::AsyncRead + Unpin> Pipe<R> {
    fn new(reader: R) -> Self {
        Self {
            reader: BufReader::new(reader),
            partial: Vec::new(),
        }
    }
}

/// The next line from a pipe, without its newline; `None` once it is
/// closed, and forever after, so a closed pipe's `select!` arm sleeps.
async fn next_line<R: tokio::io::AsyncRead + Unpin>(pipe: &mut Option<Pipe<R>>) -> Option<String> {
    let Some(Pipe { reader, partial }) = pipe else {
        return std::future::pending().await;
    };
    // `fill_buf` and `consume` rather than `read_until`, so that between
    // two awaits every byte taken from the reader is already in `partial`.
    loop {
        let available: &[u8] = match reader.fill_buf().await {
            Ok([]) | Err(_) if partial.is_empty() => return None,
            // Closed with a line still unfinished: that is the last line.
            Ok([]) | Err(_) => &[],
            Ok(available) => available,
        };
        let newline = available.iter().position(|&b| b == b'\n');
        let taken = newline.map_or(available.len(), |i| i + 1);
        partial.extend_from_slice(&available[..taken]);
        Pin::new(&mut *reader).consume(taken);
        if newline.is_none() && taken > 0 {
            continue;
        }
        let mut line = std::mem::take(partial);
        if line.ends_with(b"\n") {
            line.pop();
        }
        if line.ends_with(b"\r") {
            line.pop();
        }
        return Some(String::from_utf8_lossy(&line).into_owned());
    }
}

struct Watch<'a> {
    monitors: &'a Monitors,
    id: MonitorId,
    log: Option<File>,
    started: Instant,
    /// Stdout lines so far.
    events: usize,
    /// When the stdout lines within the flood window came.
    recent: VecDeque<Instant>,
}

impl Watch<'_> {
    /// Logs and reports one line; the monitor's end when it should stop.
    async fn line(&mut self, line: String, stream: Stream) -> Option<MonitorEnd> {
        let prefix = match stream {
            Stream::Stdout => "",
            Stream::Stderr => "stderr: ",
        };
        self.write_log(&format!("{prefix}{line}")).await;
        if stream == Stream::Stdout {
            self.events += 1;
            let now = Instant::now();
            self.recent.push_back(now);
            while self
                .recent
                .front()
                .is_some_and(|&t| now.duration_since(t) > FLOOD_WINDOW)
            {
                self.recent.pop_front();
            }
            if self.recent.len() > FLOOD_LINES {
                return Some(MonitorEnd::Flooded);
            }
        }
        let output = MonitorEvent::Output {
            id: self.id,
            line,
            stream,
        };
        if !self.monitors.event(output).await {
            return Some(MonitorEnd::Stopped(StoppedBy::Exit));
        }
        None
    }

    /// One line in the log, stamped with the time since the start. A log
    /// that fails to write is given up rather than ending the watch.
    async fn write_log(&mut self, text: &str) {
        let Some(log) = &mut self.log else { return };
        let elapsed = self.started.elapsed().as_secs_f64();
        let entry = format!("[{elapsed:>9.3}s] {text}\n");
        if log.write_all(entry.as_bytes()).await.is_err() {
            self.log = None;
        }
    }

    /// Waits for the log's writes to land: a tokio `File` hands each write
    /// to a background task, so without this whoever reads the log on
    /// `Ended` can miss its last lines.
    async fn flush_log(&mut self) {
        if let Some(log) = &mut self.log {
            let _ = log.flush().await;
        }
    }
}

/// Stops a monitor the model started.
pub struct MonitorStop;

#[derive(Deserialize)]
struct StopArgs {
    id: MonitorId,
}

impl Tool for MonitorStop {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "monitor_stop",
            description: include_str!("stop.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "minimum": 1, "description": "The monitor's id, from the monitor tool's result" }
                },
                "required": ["id"]
            }),
        }
    }

    fn call<'a>(
        &'a self,
        args: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, ToolResult> {
        async move {
            let StopArgs { id } = crate::parse_args(args)?;
            if ctx.monitors.stop(id, StoppedBy::Model) {
                Ok(format!("Stopped monitor {id}."))
            } else {
                Err(format!("there is no running monitor {id}"))
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use nth_protocol::Inbox;
    use tokio::sync::mpsc;

    use super::*;

    struct Fixture {
        dir: tempfile::TempDir,
        ctx: ToolContext,
        events: mpsc::Receiver<MonitorEvent>,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let (tx, events) = mpsc::channel(256);
        let ctx = ToolContext {
            inbox: Inbox::new(),
            ..ToolContext::new(dir.path().to_path_buf())
        };
        let ctx = ToolContext {
            monitors: Monitors::new(tx, dir.path().join("logs"), ctx.inbox.clone()),
            ..ctx
        };
        Fixture { dir, ctx, events }
    }

    async fn start(ctx: &ToolContext, command: &str, timeout_ms: u64) -> ToolResult {
        let args = json!({ "command": command, "description": "test", "timeout_ms": timeout_ms });
        Monitor.call(args, ctx).await
    }

    /// Everything the monitor reports until it ends.
    async fn until_ended(events: &mut mpsc::Receiver<MonitorEvent>) -> Vec<MonitorEvent> {
        let mut seen = Vec::new();
        while let Some(event) = events.recv().await {
            let ended = matches!(event, MonitorEvent::Ended { .. });
            seen.push(event);
            if ended {
                break;
            }
        }
        seen
    }

    fn stdout(seen: &[MonitorEvent]) -> Vec<&str> {
        seen.iter()
            .filter_map(|e| match e {
                MonitorEvent::Output {
                    line,
                    stream: Stream::Stdout,
                    ..
                } => Some(line.as_str()),
                _ => None,
            })
            .collect()
    }

    fn end(seen: &[MonitorEvent]) -> (MonitorEnd, usize) {
        match seen.last() {
            Some(MonitorEvent::Ended { end, events, .. }) => (*end, *events),
            other => panic!("not ended: {other:?}"),
        }
    }

    #[tokio::test]
    async fn streams_stdout_lines_until_the_command_exits() {
        let mut f = fixture();
        let result = start(&f.ctx, "echo one; echo oops >&2; echo two; exit 3", 10_000)
            .await
            .expect("starts");
        assert!(
            result.starts_with("Started monitor 1 (\"test\")"),
            "{result}"
        );
        assert!(result.contains("logs/1.log"), "{result}");

        let seen = until_ended(&mut f.events).await;
        assert!(matches!(seen[0], MonitorEvent::Started { id: 1, .. }));
        assert_eq!(stdout(&seen), ["one", "two"]);
        assert_eq!(end(&seen), (MonitorEnd::Exited(Some(3)), 2));

        let log = std::fs::read_to_string(f.dir.path().join("logs/1.log")).expect("log");
        assert!(log.contains("] one\n"), "{log}");
        assert!(log.contains("] stderr: oops\n"), "{log}");
        assert!(log.contains("] exited with code 3\n"), "{log}");
        let notices = f.ctx.inbox.take_notices().expect("notices");
        assert!(notices.contains("\none\ntwo\n</monitor>"), "{notices}");
        assert!(!notices.contains("oops"), "stderr is only in the log");
        assert_eq!(f.ctx.monitors.running(), 0);
    }

    #[tokio::test]
    async fn a_line_interrupted_by_stderr_arrives_whole() {
        let mut f = fixture();
        // Stdout starts a line, stderr wins the next `select!`, then stdout
        // finishes the line.
        let command =
            "printf par; sleep 0.1; echo oops >&2; sleep 0.1; echo tial; printf 'no newline'";
        start(&f.ctx, command, 10_000).await.expect("starts");
        let seen = until_ended(&mut f.events).await;
        assert_eq!(stdout(&seen), ["partial", "no newline"]);
        assert_eq!(end(&seen), (MonitorEnd::Exited(Some(0)), 2));
    }

    /// Through `kill -0`, as macOS has no `/proc`.
    fn alive(pid: &str) -> bool {
        std::process::Command::new("kill")
            .args(["-0", pid])
            .status()
            .expect("run kill")
            .success()
    }

    #[tokio::test]
    async fn background_processes_outlive_a_natural_exit() {
        let mut f = fixture();
        let pid_file = f.dir.path().join("pid");
        let command = format!("sleep 30 & echo $! > {}; echo started", pid_file.display());
        start(&f.ctx, &command, 10_000).await.expect("starts");
        let seen = until_ended(&mut f.events).await;
        assert_eq!(end(&seen), (MonitorEnd::Exited(Some(0)), 1));

        let pid = std::fs::read_to_string(&pid_file).expect("pid");
        let pid = pid.trim();
        let alive = alive(pid);
        // Our business is over either way; the sleep must not linger past
        // the test.
        let _ = std::process::Command::new("kill").arg(pid).status();
        assert!(alive, "the background sleep {pid} died with the monitor");
    }

    #[tokio::test]
    async fn times_out() {
        let mut f = fixture();
        start(&f.ctx, "sleep 30", 1_000).await.expect("starts");
        let seen = until_ended(&mut f.events).await;
        assert_eq!(end(&seen), (MonitorEnd::TimedOut { after_ms: 1_000 }, 0));
    }

    #[tokio::test]
    async fn stops_a_flood() {
        let mut f = fixture();
        start(&f.ctx, "yes", 10_000).await.expect("starts");
        let seen = until_ended(&mut f.events).await;
        assert_eq!(end(&seen).0, MonitorEnd::Flooded);
    }

    #[tokio::test]
    async fn monitor_stop_kills_the_whole_group() {
        let mut f = fixture();
        let pid_file = f.dir.path().join("pid");
        let command = format!(
            "sleep 30 & echo $! > {}; echo started; wait",
            pid_file.display()
        );
        start(&f.ctx, &command, 10_000).await.expect("starts");
        // Started, then the first line: the child is up.
        f.events.recv().await;
        f.events.recv().await;
        let pid = std::fs::read_to_string(&pid_file).expect("pid");

        let stopped = MonitorStop.call(json!({ "id": 1 }), &f.ctx).await;
        assert_eq!(stopped, Ok("Stopped monitor 1.".into()));
        let seen = until_ended(&mut f.events).await;
        assert_eq!(end(&seen).0, MonitorEnd::Stopped(StoppedBy::Model));

        // Killed, it lingers until init reaps it.
        for _ in 0..50 {
            if !alive(pid.trim()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive(pid.trim()), "the background sleep is killed too");
        let again = MonitorStop.call(json!({ "id": 1 }), &f.ctx).await;
        assert_eq!(again, Err("there is no running monitor 1".into()));
    }

    #[tokio::test]
    async fn headless_cannot_monitor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ctx = ToolContext::new(dir.path().to_path_buf());
        let out = start(&ctx, "echo hi", 1_000).await;
        assert!(out.expect_err("headless").contains("no front-end"));
    }
}
