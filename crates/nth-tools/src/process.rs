//! Running shell commands so that everything they start can be killed:
//! shared by bash and the worktree setup script, which wait for their
//! command (`run_to_exit`, keeping a `Tail` of what it printed), and
//! monitor, which leaves its command running.

use std::{
    collections::VecDeque,
    io,
    path::Path,
    process::{ExitStatus, Stdio},
    time::Duration,
};

pub(crate) use nth_protocol::reject_old_timeout;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::{Child, Command},
};

use crate::command_output;

/// The shortest timeout bash and monitor accept from the model. A `0` is
/// a mistake, not a wish to kill the command before it starts.
pub(crate) const MIN_TIMEOUT_MS: u64 = 1_000;

/// How long to keep reading after the command exits, for output still in
/// its pipes.
pub(crate) const DRAIN: Duration = Duration::from_millis(100);

/// `bash -c script` in `cwd`, with no stdin, in its own process group so a
/// timeout or a stop can kill everything the script started, not just bash.
///
/// `session` is the id of the session the command runs for, and goes out
/// as `AGENT_BROWSER_SESSION`: the agent-browser CLI the `browser` skill
/// drives keeps one browser per session name, so every command of a
/// session reaches the same browser, the user's `!` commands included,
/// while parallel sessions and subagents, each with an id of its own,
/// never share one. Nothing is set outside a session.
pub(crate) fn shell(script: &str, cwd: &Path, session: &str) -> Command {
    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .env("AGENT", "1")
        .env("AI_AGENT", "nth")
        .stdin(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    if !session.is_empty() {
        command.env("AGENT_BROWSER_SESSION", format!("nth-{session}"));
    }
    command
}

/// Kills the command's whole group when dropped while armed: a bash call
/// dropped mid-run, as when a front-end quits during a turn, or a monitor
/// that ends any way but by its command exiting. `kill_on_drop` alone
/// reaches only bash, not what it started. `run_to_exit` and monitor
/// disarm it (set it to `None`) once the command has exited and been
/// reaped, so processes it put in the background live on, and so the kill
/// never names a pid that could have been reused.
pub(crate) struct KillGroupOnDrop(pub(crate) Option<u32>);

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            kill_pid_group(pid);
        }
    }
}

pub(crate) fn kill_group(child: &Child) {
    if let Some(pid) = child.id() {
        kill_pid_group(pid);
    }
}

fn kill_pid_group(pid: u32) {
    let Ok(pgid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: kill has no memory-safety preconditions. The group id is the
    // pid of our own child, and both callers only pass it while the child
    // is unreaped (`Child::id` is `None` once it has been waited for, and
    // the guard is disarmed then), so it cannot have been reused.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// How much of a command's output one read takes.
const CHUNK: usize = 64 * 1024;

/// The end of a command's output, all the model is shown of it, kept as
/// it arrives: a chatty command holds at most `cap` bytes in memory, not
/// everything it printed before the cut.
pub(crate) struct Tail {
    buf: VecDeque<u8>,
    /// Bytes, enough for `max_chars` characters of any width.
    cap: usize,
    max_chars: usize,
    dropped: u64,
}

impl Tail {
    pub(crate) fn new(max_chars: usize) -> Self {
        let cap = max_chars.saturating_mul(4).max(4);
        Self {
            buf: VecDeque::new(),
            cap,
            max_chars,
            dropped: 0,
        }
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        // A chunk larger than the whole ring replaces it: copying the bytes
        // it would push straight out again is wasted work.
        let bytes = match bytes.len().checked_sub(self.cap) {
            Some(skip) => {
                self.dropped += (self.buf.len() + skip) as u64;
                self.buf.clear();
                &bytes[skip..]
            }
            None => bytes,
        };
        self.buf.extend(bytes);
        let excess = self.buf.len().saturating_sub(self.cap);
        if excess > 0 {
            self.buf.drain(..excess);
            self.dropped += excess as u64;
        }
        // Start on a character, so the model never sees half of one. Only
        // after a cut: output that starts mid-character came that way.
        if self.dropped > 0 {
            while self.buf.front().is_some_and(|&b| b & 0xC0 == 0x80) {
                self.buf.pop_front();
                self.dropped += 1;
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.buf.len()
    }

    /// What the model is shown: the kept output without its escapes, cut
    /// to `max_chars`, with a note when anything was left out.
    pub(crate) fn text(&mut self) -> String {
        let clean = command_output::clean(self.buf.make_contiguous());
        let count = clean.chars().count();
        if self.dropped == 0 && count <= self.max_chars {
            return clean;
        }
        let skip = count.saturating_sub(self.max_chars);
        let start = clean
            .char_indices()
            .nth(skip)
            .map_or(clean.len(), |(i, _)| i);
        let mut shown = &clean[start..];
        // A cut, by the ring or by characters, leaves half a line at the
        // front (after a ring cut maybe the rest of an escape, showing as
        // text): start at the next whole line, unless the cut already fell
        // on one or nothing visible follows, so the model is never left
        // with nothing.
        let on_a_line = start > 0 && clean.as_bytes()[start - 1] == b'\n';
        if !on_a_line
            && let Some(newline) = shown.find('\n')
            && !shown[newline + 1..].trim().is_empty()
        {
            shown = &shown[newline + 1..];
        }
        let shown_count = shown.chars().count();
        format!("...output truncated, showing the last {shown_count} characters...\n{shown}")
    }
}

/// What `run_to_exit` hands a command's output to as it arrives. A trait
/// rather than an async closure, whose future the compiler cannot prove
/// `Send` for every lifetime of the bytes it borrows.
pub(crate) trait Receiver {
    fn receive(&mut self, bytes: &[u8]) -> impl Future<Output = ()> + Send;
}

impl Receiver for Tail {
    fn receive(&mut self, bytes: &[u8]) -> impl Future<Output = ()> + Send {
        self.push(bytes);
        std::future::ready(())
    }
}

/// How a command `run_to_exit` waited for ended.
#[derive(Debug)]
pub(crate) enum Outcome {
    Exited(ExitStatus),
    TimedOut,
    /// Reading its output or waiting for it failed.
    Failed(io::Error),
}

/// Hands the command's output to `receiver` until bash itself exits, rather
/// than until stdout closes, which processes it put in the background can
/// delay forever; within `timeout` when there is one. Anything but `Exited`
/// kills the command's whole group, and so does dropping the future.
pub(crate) async fn run_to_exit(
    child: &mut Child,
    stdout: &mut (impl AsyncRead + Unpin),
    timeout: Option<Duration>,
    receiver: &mut impl Receiver,
) -> Outcome {
    let mut abandoned = KillGroupOnDrop(child.id());
    let mut chunk = vec![0; CHUNK];
    let exit = async {
        loop {
            tokio::select! {
                read = stdout.read(&mut chunk) => match read {
                    Ok(0) => return child.wait().await,
                    Ok(n) => receiver.receive(&chunk[..n]).await,
                    Err(e) => return Err(e),
                },
                status = child.wait() => return status,
            }
        }
    };
    let ran = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, exit).await.ok(),
        None => Some(exit.await),
    };
    let outcome = match ran {
        Some(Ok(status)) => Outcome::Exited(status),
        Some(Err(e)) => Outcome::Failed(e),
        None => Outcome::TimedOut,
    };
    // Disarmed only once bash has been reaped: then whatever it left in the
    // background is meant to live on, and its pid may be reused. Any other
    // way out leaves the group running, so it is killed here, while bash is
    // still unreaped and the group id still ours.
    if !matches!(outcome, Outcome::Exited(_)) {
        kill_group(child);
    }
    abandoned.0 = None;
    // Processes the command put in the background may hold the pipe open
    // indefinitely; take what they already wrote and move on. After a kill
    // this takes up to DRAIN too, collecting what the group wrote before it.
    let _ = tokio::time::timeout(DRAIN, async {
        while let Ok(n @ 1..) = stdout.read(&mut chunk).await {
            receiver.receive(&chunk[..n]).await;
        }
    })
    .await;
    outcome
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };

    use tokio::io::ReadBuf;

    use super::*;
    use crate::output;

    const DEADLINE: Duration = Duration::from_secs(10);

    /// Waits until `pid` has exited (or is a zombie no one reaped yet).
    pub(crate) async fn assert_process_gone(pid: &str) {
        tokio::time::timeout(DEADLINE, async {
            loop {
                let alive = std::process::Command::new("kill")
                    .args(["-0", pid.trim()])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
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

    #[test]
    fn a_tail_keeps_the_last_bytes_and_counts_the_rest() {
        // Room for 2 characters is 8 bytes.
        let mut tail = Tail::new(2);
        tail.push(b"abcdef");
        assert_eq!(
            tail.text(),
            "...output truncated, showing the last 2 characters...\nef"
        );
        tail.push(b"ghij");
        assert_eq!(tail.len(), 8);
        assert_eq!(
            tail.text(),
            "...output truncated, showing the last 2 characters...\nij"
        );
        // A chunk larger than the ring replaces it.
        tail.push(b"0123456789xy");
        assert_eq!(tail.len(), 8);
        assert_eq!(
            tail.text(),
            "...output truncated, showing the last 2 characters...\nxy"
        );
    }

    #[test]
    fn a_tail_starts_on_a_character() {
        // Room for 1 character is 4 bytes: the cut lands inside the é.
        let mut tail = Tail::new(1);
        tail.push("aé€".as_bytes());
        assert_eq!(tail.len(), 3);
        assert_eq!(
            tail.text(),
            "...output truncated, showing the last 1 characters...\n€"
        );
    }

    #[test]
    fn a_cut_tail_starts_at_a_whole_line() {
        // Output that is mostly escapes, so the front of the ring is within
        // the characters shown, and a cut that lands inside `\x1b[31m`.
        let mut tail = Tail::new(output::MAX_CHARS);
        let cap = output::MAX_CHARS * 4;
        let line = "\x1b[32m€\x1b[0m\n";
        let body = format!("{}abcdefghijk\n", line.repeat(9_229));
        assert_eq!(body.len(), cap - "31mred\x1b[0m\n".len());
        tail.push(&b"z\n".repeat(10));
        tail.push(b"\x1b[31mred\x1b[0m\n");
        tail.push(body.as_bytes());
        assert_eq!(tail.len(), cap);

        let shown = format!("{}abcdefghijk\n", "€\n".repeat(9_229));
        assert_eq!(
            tail.text(),
            format!(
                "...output truncated, showing the last {} characters...\n{shown}",
                shown.chars().count()
            )
        );
    }

    #[test]
    fn a_long_plain_output_starts_at_a_whole_line() {
        // The ring holds 48 bytes, so the character cut to 12 lands inside
        // `line 098`.
        let mut tail = Tail::new(12);
        for n in 0..100 {
            tail.push(format!("line {n:03}\n").as_bytes());
        }
        assert_eq!(
            tail.text(),
            "...output truncated, showing the last 9 characters...\nline 099\n"
        );
    }

    #[test]
    fn a_tail_that_ends_in_escapes_still_shows_its_last_line() {
        let mut tail = Tail::new(2);
        tail.push(b"abcdefgh\n\x1b[0m");
        assert_eq!(
            tail.text(),
            "...output truncated, showing the last 2 characters...\nh\n"
        );
    }

    #[test]
    fn a_cut_tail_without_a_later_line_keeps_its_last_line() {
        let mut tail = Tail::new(2);
        tail.push(b"0123456789abc\n");
        assert_eq!(
            tail.text(),
            "...output truncated, showing the last 2 characters...\nc\n"
        );
    }

    /// Passes the command's output on until some has arrived, then fails as
    /// a broken pipe would.
    struct FailsAfterOutput<R> {
        inner: R,
        seen: bool,
    }

    impl<R: AsyncRead + Unpin> AsyncRead for FailsAfterOutput<R> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.seen {
                return Poll::Ready(Err(io::Error::other("read failed")));
            }
            let before = buf.filled().len();
            let polled = Pin::new(&mut self.inner).poll_read(cx, buf);
            if buf.filled().len() > before {
                self.seen = true;
            }
            polled
        }
    }

    #[tokio::test]
    async fn a_pipe_read_error_still_kills_the_group() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut child = shell("sleep 30 & echo $! > pid; echo ready; wait", dir.path(), "")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn");
        let stdout = child.stdout.take().expect("piped");
        let mut stdout = FailsAfterOutput {
            inner: stdout,
            seen: false,
        };

        let mut tail = Tail::new(100);
        let outcome = run_to_exit(&mut child, &mut stdout, Some(DEADLINE), &mut tail).await;

        assert!(matches!(outcome, Outcome::Failed(_)), "{outcome:?}");
        let pid = std::fs::read_to_string(dir.path().join("pid")).expect("pid file");
        assert_process_gone(&pid).await;
    }

    #[tokio::test]
    async fn commands_receive_agent_and_session_environment() {
        let output = shell(
            "printf '%s %s %s' \"$AGENT\" \"$AI_AGENT\" \"$AGENT_BROWSER_SESSION\"",
            Path::new("."),
            "abc",
        )
        .output()
        .await
        .unwrap();
        assert_eq!(output.stdout, b"1 nth nth-abc");
    }

    #[tokio::test]
    async fn tools_reject_old_timeout_names_even_with_new_name() {
        use std::sync::Arc;

        use nth_protocol::{Tool, ToolContext};
        use serde_json::json;
        let ctx = ToolContext::new(Path::new(".").to_path_buf());
        let tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(crate::bash::Bash::default()),
            Arc::new(crate::webfetch::WebFetch),
            Arc::new(crate::monitor::Monitor),
        ];
        for tool in tools {
            for old in ["timeout", "timeout_secs"] {
                let mut args = json!({"command":"true", "description":"test", "url":"http://localhost", "timeout_ms":1000});
                args[old] = json!(1);
                let err = tool.call(args, &ctx).await.unwrap_err();
                assert!(err.contains("use timeout_ms"), "{err}");
            }
        }
    }
}
