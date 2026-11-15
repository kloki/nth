//! A monitor's stdin: `monitor_input`, the tool the model writes to it
//! with, and the task that owns the pipe and writes it. A task of its own
//! rather than an arm of the watch's `select!`: a command that does not
//! read fills the pipe, and a write waiting on it there would hold up
//! reading the command's output behind it.

use std::{
    io,
    os::fd::{AsFd, OwnedFd},
    sync::{Arc, Mutex},
    time::Duration,
};

use futures::{FutureExt, future::BoxFuture};
use nth_protocol::{
    InputRefused, MonitorId, Monitors, StdinWrite, Tool, ToolContext, ToolResult, ToolSpec,
};
use serde::Deserialize;
use serde_json::json;
use tokio::{
    io::AsyncWriteExt,
    process::{Child, ChildStdin},
    sync::mpsc,
    task::JoinHandle,
};

use super::MAX_LINE_BYTES;
use crate::command_output;

pub(super) const NAME: &str = "monitor_input";

/// Writes to a monitor's stdin waiting for the command to read, past which
/// `monitor_input` refuses more rather than queue without end.
const QUEUED_WRITES: usize = 8;
/// How long `monitor_input` waits for its text to be written before it
/// answers that the command has not read it yet, so a command that stopped
/// reading never holds up the model's turn.
const WRITE_WAIT: Duration = Duration::from_secs(5);
/// Writes whose echo the watch has yet to log and show.
const ECHOED_WRITES: usize = 8;
/// The most lines of one write the tab and the log show; the rest are
/// counted in one line after them. The model wrote them, so it has them.
const ECHOED_LINES: usize = 20;

/// The lines one write shows in the tab and the log, cut and cleaned.
pub(super) type Echo = Vec<String>;

/// A monitor's open stdin, before its writer starts.
pub(super) struct Input {
    pipe: ChildStdin,
    held: Held,
    writes: mpsc::Receiver<StdinWrite>,
}

impl Input {
    /// Takes the stdin of a command spawned with it piped, with a second
    /// handle on it, and returns the sender the model's writes queue on.
    pub(super) fn open(child: &mut Child) -> Result<(mpsc::Sender<StdinWrite>, Self), String> {
        let pipe = child
            .stdin
            .take()
            .ok_or("bash was started without a stdin pipe")?;
        let held = pipe
            .as_fd()
            .try_clone_to_owned()
            .map_err(|e| format!("failed to hold stdin open: {e}"))?;
        let (queue, writes) = mpsc::channel(QUEUED_WRITES);
        let input = Self {
            pipe,
            held: Held(Arc::new(Mutex::new(Some(held)))),
            writes,
        };
        Ok((queue, input))
    }

    /// The handle that keeps the pipe open once the writer has stopped.
    pub(super) fn held(&self) -> Held {
        self.held.clone()
    }
}

/// A second handle on a monitor's stdin pipe, so that the command reads
/// end-of-input only when the model closes it, never because the writer
/// stopped: a monitor that is stopped, times out or floods mid-write must
/// be killed with a write cut short, not handed the half of it that went
/// in as a whole (half a manifest to `kubectl apply -f -`). The watch
/// releases it once the command is killed, or has exited by itself, and a
/// close from the model releases it at once, with the writer's own handle.
#[derive(Clone)]
pub(super) struct Held(Arc<Mutex<Option<OwnedFd>>>);

impl Held {
    pub(super) fn release(&self) {
        self.0.lock().expect("held stdin lock poisoned").take();
    }
}

/// Why a monitor may not be started with its stdin open: the agent the
/// session runs as cannot write to it, so the command would wait on it
/// with nobody to close it.
pub(super) fn refused(ctx: &ToolContext) -> Option<String> {
    ctx.agent
        .as_ref()
        .filter(|agent| !agent.allows(NAME))
        .map(|agent| {
            format!(
                "the {} agent cannot use {NAME}, so nothing could write to this monitor's stdin; start it without stdin",
                agent.name
            )
        })
}

/// The writer task. The watch stops it as the monitor ends, and dropping
/// it aborts the task, so it never outlives the watch: a write still
/// waiting on a command that never read it, or on what the command left in
/// the background holding the pipe, goes with it.
pub(super) struct Writer(JoinHandle<()>);

impl Writer {
    /// Starts the task that writes the model's writes to the pipe one after
    /// another, with the receiver each one's echo comes over once it is
    /// written.
    pub(super) fn spawn(
        monitors: Monitors,
        id: MonitorId,
        input: Input,
    ) -> (Self, mpsc::Receiver<Echo>) {
        let (echo, echoes) = mpsc::channel(ECHOED_WRITES);
        let task = tokio::spawn(write(monitors, id, input, echo));
        (Self(task), echoes)
    }

    /// Stops the task and waits until it is gone: nothing is written,
    /// answered or echoed after this. The pipe stays open while the
    /// [`Held`] handle does.
    pub(super) async fn stop(mut self) {
        self.0.abort();
        let _ = (&mut self.0).await;
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Ends after a close, dropping the pipe and releasing the held handle so
/// the command reads end-of-input, or after a failed write; writes still
/// queued then are dropped unanswered.
async fn write(monitors: Monitors, id: MonitorId, input: Input, echo: mpsc::Sender<Echo>) {
    let Input {
        mut pipe,
        held,
        mut writes,
    } = input;
    while let Some(StdinWrite { text, close, done }) = writes.recv().await {
        // Room for the echo first, so that once the text is written its
        // echo and its answer go out with no await between them: stopped
        // at any point, the task leaves no write answered and not echoed.
        let Ok(room) = echo.reserve().await else {
            return;
        };
        let written = async {
            pipe.write_all(text.as_bytes()).await?;
            pipe.flush().await
        }
        .await;
        if let Err(e) = written {
            let reason = reason(&e);
            monitors.stdin_failed(id, reason.clone());
            let _ = done.send(Err(reason));
            return;
        }
        room.send(echo_of(&text));
        let _ = done.send(Ok(()));
        if close {
            held.release();
            return;
        }
    }
}

fn reason(error: &io::Error) -> String {
    match error.kind() {
        io::ErrorKind::BrokenPipe => "the command stopped reading its stdin".into(),
        _ => error.to_string(),
    }
}

/// What the tab and the log show of a write: its first lines, each cut at
/// [`MAX_LINE_BYTES`] and marked as a cut stdout line is, without escapes
/// or control characters, as the command's own output is shown.
fn echo_of(text: &str) -> Echo {
    let mut echo: Echo = text.lines().take(ECHOED_LINES).map(echo_line).collect();
    match text.lines().count().saturating_sub(ECHOED_LINES) {
        0 => {}
        1 => echo.push("… 1 more line".into()),
        more => echo.push(format!("… {more} more lines")),
    }
    echo
}

fn echo_line(line: &str) -> String {
    let mut end = line.len().min(MAX_LINE_BYTES);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    let mut shown = command_output::clean(&line.as_bytes()[..end]);
    if end < line.len() {
        shown.push('…');
    }
    shown
}

/// The next write's echo; never once the writer has gone and every echo
/// was taken, so the watch's `select!` arm sleeps, as a closed pipe's does.
pub(super) async fn echoed(echoes: &mut Option<mpsc::Receiver<Echo>>) -> Echo {
    if let Some(rx) = echoes {
        if let Some(echo) = rx.recv().await {
            return echo;
        }
        *echoes = None;
    }
    std::future::pending().await
}

/// Writes to the stdin of a monitor started with one.
pub struct MonitorInput;

#[derive(Deserialize)]
struct InputArgs {
    id: MonitorId,
    #[serde(default)]
    text: String,
    #[serde(default)]
    close: bool,
}

impl Tool for MonitorInput {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: NAME,
            description: include_str!("input.txt").into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "integer", "minimum": 1, "description": "The monitor's id, from the monitor tool's result" },
                    "text": { "type": "string", "description": "Written as it is. End each line with \\n, as pressing Enter would." },
                    "close": { "type": "boolean", "description": "Close stdin after text, or alone when text is empty, so the command reads end-of-input. Default false." }
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
            let InputArgs { id, text, close } = nth_protocol::parse_args(args)?;
            if text.is_empty() && !close {
                return Err("nothing to write: give text, or close".into());
            }
            let bytes = text.len();
            let written = ctx
                .monitors
                .input(id, text, close)
                .map_err(|refused| refused_input(id, refused))?;
            match tokio::time::timeout(WRITE_WAIT, written).await {
                Ok(Ok(Ok(()))) => Ok(match (bytes, close) {
                    (0, _) => format!("Closed monitor {id}'s stdin."),
                    (_, false) => format!("Wrote {bytes} bytes to monitor {id}'s stdin."),
                    (_, true) => {
                        format!("Wrote {bytes} bytes to monitor {id}'s stdin and closed it.")
                    }
                }),
                Ok(Ok(Err(reason))) => Err(format!(
                    "writing to monitor {id}'s stdin failed: {reason}"
                )),
                // Dropped unanswered: behind a write that failed, or by a
                // writer the watch stopped, maybe partway through it.
                Ok(Err(_)) => Err(match ctx.monitors.stdin_failure(id) {
                    Some(reason) => format!(
                        "an earlier write to monitor {id}'s stdin failed, so this was not written: {reason}"
                    ),
                    None => {
                        format!("monitor {id} ended before all of this was written to its stdin")
                    }
                }),
                Err(_) => Ok(match (bytes, close) {
                    (0, _) => format!(
                        "Monitor {id} has not read what was written before; its stdin closes once it has."
                    ),
                    (_, false) => format!(
                        "Monitor {id} has not read it yet; it goes in when the command reads."
                    ),
                    (_, true) => format!(
                        "Monitor {id} has not read it yet; it goes in when the command reads, and then its stdin closes."
                    ),
                }),
            }
        }
        .boxed()
    }
}

fn refused_input(id: MonitorId, refused: InputRefused) -> String {
    match refused {
        InputRefused::NoMonitor => format!("there is no running monitor {id}"),
        InputRefused::NoStdin => format!(
            "monitor {id} was started without stdin; start it again with stdin: true to write to it"
        ),
        InputRefused::Closed => format!("monitor {id}'s stdin is closed: you closed it"),
        InputRefused::Failed(reason) => {
            format!("writing to monitor {id}'s stdin failed: {reason}")
        }
        InputRefused::Backlogged => format!(
            "monitor {id} has not read what you wrote before; wait for it to read, or stop it"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use nth_protocol::{MonitorEnd, MonitorEvent, StoppedBy, Stream};
    use tokio::sync::oneshot;

    use super::{
        super::{
            FLOOD_LINES, Monitor, MonitorStop,
            tests::{end, fixture, start, stdout, until_ended},
        },
        *,
    };

    /// Whether `path` appears within `within`.
    async fn appears(path: &std::path::Path, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            if path.exists() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        path.exists()
    }

    /// Records end-of-input in `marker` once `cat` reads it.
    const RECORDS_EOF: &str = "cat > out; echo eof-seen > marker";

    #[tokio::test]
    async fn stopping_the_writer_leaves_the_pipe_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut child = crate::process::shell(RECORDS_EOF, dir.path(), "")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn");
        let (queue, input) = Input::open(&mut child).expect("stdin");
        let held = input.held();
        let (writer, _echoes) = Writer::spawn(Monitors::default(), 1, input);
        let (done, written) = oneshot::channel();
        let write = StdinWrite {
            text: "a\n".into(),
            close: false,
            done,
        };
        queue.send(write).await.expect("queued");
        assert_eq!(written.await, Ok(Ok(())));

        writer.stop().await;
        let marker = dir.path().join("marker");
        assert!(
            !appears(&marker, Duration::from_millis(300)).await,
            "the command read end-of-input while the pipe was held"
        );
        held.release();
        assert!(appears(&marker, Duration::from_secs(5)).await, "released");
        child.wait().await.expect("exits");
        let out = std::fs::read_to_string(dir.path().join("out")).expect("out");
        assert_eq!(out, "a\n");
    }

    #[tokio::test]
    async fn a_stopped_monitor_never_reads_end_of_input() {
        let mut f = fixture();
        start_with_stdin(&f.ctx, RECORDS_EOF).await.expect("starts");
        input(&f.ctx, 1, "a\n", false).await.expect("written");

        MonitorStop
            .call(json!({ "id": 1 }), &f.ctx)
            .await
            .expect("stops");
        let seen = until_ended(&mut f.events).await;
        assert_eq!(end(&seen).0, MonitorEnd::Stopped(StoppedBy::Model));
        let marker = f.dir.path().join("marker");
        assert!(
            !appears(&marker, Duration::from_millis(300)).await,
            "killed, not closed"
        );
    }

    #[tokio::test]
    async fn close_delivers_end_of_input_while_the_monitor_runs() {
        let mut f = fixture();
        start_with_stdin(&f.ctx, "cat > /dev/null; echo eof-seen; sleep 30")
            .await
            .expect("starts");
        assert_eq!(
            input(&f.ctx, 1, "", true).await,
            Ok("Closed monitor 1's stdin.".into())
        );
        let heard = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match f.events.recv().await {
                    Some(MonitorEvent::Output { line, .. }) if line == "eof-seen" => return,
                    Some(MonitorEvent::Ended { .. }) | None => panic!("ended first"),
                    Some(_) => {}
                }
            }
        })
        .await;
        assert!(heard.is_ok(), "the close never reached the command");
        f.ctx.monitors.stop(1, StoppedBy::Model);
        until_ended(&mut f.events).await;
    }

    async fn start_with_stdin(ctx: &ToolContext, command: &str) -> ToolResult {
        let args = json!({ "command": command, "description": "test", "timeout_ms": 30_000, "stdin": true });
        Monitor.call(args, ctx).await
    }

    async fn input(ctx: &ToolContext, id: MonitorId, text: &str, close: bool) -> ToolResult {
        let args = json!({ "id": id, "text": text, "close": close });
        MonitorInput.call(args, ctx).await
    }

    fn stdin(seen: &[MonitorEvent]) -> Vec<&str> {
        seen.iter()
            .filter_map(|e| match e {
                MonitorEvent::Output {
                    line,
                    stream: Stream::Stdin,
                    ..
                } => Some(line.as_str()),
                _ => None,
            })
            .collect()
    }

    /// Past any pipe buffer, so a write of it waits on a reader that never
    /// comes.
    fn past_the_pipe() -> String {
        "x".repeat(256 * 1024)
    }

    #[tokio::test]
    async fn stdin_reaches_the_command_and_close_ends_it() {
        let mut f = fixture();
        let started = start_with_stdin(&f.ctx, r#"while read l; do echo "got $l"; done"#)
            .await
            .expect("starts");
        assert!(
            started.contains("Write to its stdin with monitor_input."),
            "{started}"
        );

        let wrote = input(&f.ctx, 1, "a\nb\n", false).await;
        assert_eq!(wrote, Ok("Wrote 4 bytes to monitor 1's stdin.".into()));
        let closed = input(&f.ctx, 1, "", true).await;
        assert_eq!(closed, Ok("Closed monitor 1's stdin.".into()));

        let seen = until_ended(&mut f.events).await;
        assert_eq!(stdout(&seen), ["got a", "got b"]);
        assert_eq!(stdin(&seen), ["a", "b"]);
        assert_eq!(
            end(&seen),
            (MonitorEnd::Exited(Some(0)), 2),
            "echoes are no events"
        );
        let log = std::fs::read_to_string(f.dir.path().join("logs/1.log")).expect("log");
        assert!(log.contains("] stdin: a\n"), "{log}");
        assert!(log.contains("] got b\n"), "{log}");
    }

    #[tokio::test]
    async fn input_needs_a_stdin_monitor() {
        let mut f = fixture();
        start(&f.ctx, "sleep 30", 30_000).await.expect("starts");
        assert_eq!(
            input(&f.ctx, 1, "x\n", false).await,
            Err("monitor 1 was started without stdin; start it again with stdin: true to write to it".into())
        );
        assert_eq!(
            input(&f.ctx, 9, "x\n", false).await,
            Err("there is no running monitor 9".into())
        );
        assert_eq!(
            input(&f.ctx, 1, "", false).await,
            Err("nothing to write: give text, or close".into())
        );

        // Still running after cat read end-of-input, so the refusal is
        // about the close, not about a monitor that has gone.
        start_with_stdin(&f.ctx, "cat; sleep 30")
            .await
            .expect("starts");
        assert_eq!(
            input(&f.ctx, 2, "x\n", true).await,
            Ok("Wrote 2 bytes to monitor 2's stdin and closed it.".into())
        );
        assert_eq!(
            input(&f.ctx, 2, "y\n", false).await,
            Err("monitor 2's stdin is closed: you closed it".into())
        );

        f.ctx.monitors.stop_all(StoppedBy::Model);
        let mut ended = 0;
        while ended < 2 {
            if let Some(MonitorEvent::Ended { .. }) = f.events.recv().await {
                ended += 1;
            }
        }
    }

    #[tokio::test]
    async fn an_agent_that_cannot_write_stdin_cannot_open_it() {
        let f = fixture();
        let mut ctx = f.ctx.clone();
        ctx.agent = Some(nth_context::Agent {
            name: "watcher".into(),
            mode: nth_context::agents::Mode::Primary,
            description: None,
            prompt: None,
            tools: Some(vec!["monitor".into()]),
            denied: vec![],
            model: None,
            clerk: false,
            hidden: false,
            path: None,
            source: nth_context::agents::Source::Builtin,
            root: None,
        });
        assert_eq!(
            start_with_stdin(&ctx, "cat").await,
            Err("the watcher agent cannot use monitor_input, so nothing could write to this monitor's stdin; start it without stdin".into())
        );
        assert_eq!(ctx.monitors.running(), 0, "nothing started");
    }

    #[tokio::test]
    async fn a_command_that_does_not_read_does_not_block_the_call() {
        let mut f = fixture();
        start_with_stdin(&f.ctx, "echo up; sleep 30")
            .await
            .expect("starts");
        let flood = past_the_pipe();
        let asked = Instant::now();
        // A write the command never reads, and a close behind it.
        let (wrote, closed) = tokio::join!(input(&f.ctx, 1, &flood, false), async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            input(&f.ctx, 1, "", true).await
        });
        assert_eq!(
            wrote,
            Ok("Monitor 1 has not read it yet; it goes in when the command reads.".into())
        );
        assert_eq!(
            closed,
            Ok(
                "Monitor 1 has not read what was written before; its stdin closes once it has."
                    .into()
            )
        );
        assert!(asked.elapsed() < WRITE_WAIT + Duration::from_secs(2));

        // The watch still reads the command's output and stops.
        let stopped = MonitorStop.call(json!({ "id": 1 }), &f.ctx).await;
        assert_eq!(stopped, Ok("Stopped monitor 1.".into()));
        let seen = until_ended(&mut f.events).await;
        assert_eq!(stdout(&seen), ["up"]);
        assert!(stdin(&seen).is_empty(), "never written, never echoed");
        assert_eq!(end(&seen).0, MonitorEnd::Stopped(StoppedBy::Model));
    }

    /// Resolves once the writer has dropped the write's answer, which only
    /// a task that is gone does to a write it is still waiting on.
    async fn assert_gone(written: oneshot::Receiver<Result<(), String>>) {
        let answered = tokio::time::timeout(Duration::from_secs(5), written)
            .await
            .expect("the writer outlived its monitor");
        assert!(answered.is_err(), "answered: {answered:?}");
    }

    #[tokio::test]
    async fn the_writer_stops_with_its_monitor() {
        let mut f = fixture();
        start_with_stdin(&f.ctx, "sleep 30").await.expect("starts");
        start_with_stdin(&f.ctx, "sleep 30").await.expect("starts");
        let stuck = f.ctx.monitors.input(1, past_the_pipe(), false).unwrap();
        let forgotten = f.ctx.monitors.input(2, past_the_pipe(), false).unwrap();

        MonitorStop
            .call(json!({ "id": 1 }), &f.ctx)
            .await
            .expect("stops");
        assert_gone(stuck).await;

        f.ctx.monitors.forget_all();
        assert_gone(forgotten).await;
        let mut ended = 0;
        while ended < 2 {
            if let Some(MonitorEvent::Ended { .. }) = f.events.recv().await {
                ended += 1;
            }
        }
    }

    #[tokio::test]
    async fn a_failed_write_is_named_by_the_next_call() {
        let mut f = fixture();
        // Leaves stdin unread, then closes the last reader of it.
        start_with_stdin(&f.ctx, "sleep 0.5; exec 0<&-; sleep 30")
            .await
            .expect("starts");
        let stuck = f.ctx.monitors.input(1, past_the_pipe(), false).unwrap();
        let failed = tokio::time::timeout(Duration::from_secs(10), stuck)
            .await
            .expect("the write fails once nobody can read it");
        assert_eq!(
            failed,
            Ok(Err("the command stopped reading its stdin".into()))
        );
        assert_eq!(
            input(&f.ctx, 1, "y\n", false).await,
            Err(
                "writing to monitor 1's stdin failed: the command stopped reading its stdin".into()
            )
        );
        f.ctx.monitors.stop(1, StoppedBy::Model);
        until_ended(&mut f.events).await;
    }

    #[tokio::test]
    async fn echoes_are_cut_cleaned_and_capped() {
        let mut f = fixture();
        start_with_stdin(&f.ctx, "cat > /dev/null")
            .await
            .expect("starts");
        input(&f.ctx, 1, &past_the_pipe(), false)
            .await
            .expect("written");
        let lines: String = (1..=100).map(|n| format!("{n}\n")).collect();
        input(&f.ctx, 1, &lines, false).await.expect("written");
        input(&f.ctx, 1, "\x1b[31mred\x1b[0m\x07\n", true)
            .await
            .expect("written");

        let seen = until_ended(&mut f.events).await;
        let echoed = stdin(&seen);
        let cut = format!("{}…", "x".repeat(MAX_LINE_BYTES));
        let first: Vec<String> = (1..=ECHOED_LINES).map(|n| n.to_string()).collect();
        assert_eq!(echoed[0], cut);
        assert_eq!(echoed[1..=ECHOED_LINES], first);
        assert_eq!(echoed[ECHOED_LINES + 1], "… 80 more lines");
        assert_eq!(echoed[ECHOED_LINES + 2..], ["red"]);
    }

    #[test]
    fn an_echo_cut_keeps_whole_characters() {
        let line = "é".repeat(MAX_LINE_BYTES);
        let shown = echo_line(&line);
        assert_eq!(shown, format!("{}…", "é".repeat(MAX_LINE_BYTES / 2)));
        assert_eq!(echo_of("a\nb"), ["a", "b"]);
        let one_over: String = "x\n".repeat(ECHOED_LINES + 1);
        assert_eq!(echo_of(&one_over).last().unwrap(), "… 1 more line");
    }

    #[tokio::test]
    async fn stdin_lines_do_not_count_toward_the_flood() {
        let mut f = fixture();
        start_with_stdin(&f.ctx, "cat > /dev/null")
            .await
            .expect("starts");
        // Past the flood guard together, within its window.
        let writes = FLOOD_LINES / ECHOED_LINES + 2;
        for _ in 0..writes {
            let lines = "x\n".repeat(ECHOED_LINES);
            input(&f.ctx, 1, &lines, false).await.expect("written");
        }
        input(&f.ctx, 1, "", true).await.expect("closed");
        let seen = until_ended(&mut f.events).await;
        assert_eq!(stdin(&seen).len(), writes * ECHOED_LINES);
        assert_eq!(end(&seen), (MonitorEnd::Exited(Some(0)), 0));
    }

    #[tokio::test]
    async fn without_stdin_a_command_reads_end_of_input() {
        let mut f = fixture();
        start(&f.ctx, "cat; echo done", 30_000)
            .await
            .expect("starts");
        let seen = tokio::time::timeout(Duration::from_secs(10), until_ended(&mut f.events))
            .await
            .expect("cat reads end-of-input at once");
        assert_eq!(stdout(&seen), ["done"]);
        assert_eq!(end(&seen), (MonitorEnd::Exited(Some(0)), 1));
    }
}
