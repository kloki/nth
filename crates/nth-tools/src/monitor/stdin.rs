//! The task that writes what the model types into a monitor's stdin. A task
//! of its own rather than an arm of the watch's `select!`: a command that
//! does not read fills the pipe, and a write waiting on it there would hold
//! up reading the command's output behind it.

use std::io;

use nth_protocol::StdinWrite;
use tokio::{io::AsyncWriteExt, process::ChildStdin, sync::mpsc, task::JoinHandle};

/// The writer task, aborted when the watch that owns it ends, however it
/// ends: a write still waiting on a command that never read it, or on what
/// the command left in the background holding the pipe, goes with it.
pub(super) struct Writer(JoinHandle<()>);

impl Drop for Writer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts the task that writes `writes` to `pipe` one after another, and
/// sends what each wrote to `echo`, a line at a time, once it is written.
pub(super) fn spawn(
    pipe: ChildStdin,
    writes: mpsc::Receiver<StdinWrite>,
    echo: mpsc::Sender<String>,
) -> Writer {
    Writer(tokio::spawn(write(pipe, writes, echo)))
}

/// Ends after a close or a failed write, dropping the pipe so the command
/// reads end-of-input; writes still queued then are dropped unanswered,
/// which their callers hear as the stdin having closed.
async fn write(
    mut pipe: ChildStdin,
    mut writes: mpsc::Receiver<StdinWrite>,
    echo: mpsc::Sender<String>,
) {
    while let Some(StdinWrite { text, close, done }) = writes.recv().await {
        let written = async {
            pipe.write_all(text.as_bytes()).await?;
            pipe.flush().await
        }
        .await;
        if let Err(e) = written {
            let _ = done.send(Err(reason(&e)));
            return;
        }
        let _ = done.send(Ok(()));
        for line in text.lines() {
            // The watch is gone: nobody is left to show it to.
            if echo.send(line.to_string()).await.is_err() {
                break;
            }
        }
        if close {
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
