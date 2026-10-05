//! Running shell commands so that everything they start can be killed:
//! shared by bash, which waits for its command, and monitor, which leaves
//! its command running.

use std::{path::Path, process::Stdio};

use tokio::process::{Child, Command};

/// The shortest timeout bash and monitor accept from the model. A `0` is
/// a mistake, not a wish to kill the command before it starts.
pub(crate) const MIN_TIMEOUT_MS: u64 = 1_000;

/// `bash -c script` in `cwd`, with no stdin, in its own process group so a
/// timeout or a stop can kill everything the script started, not just bash.
pub(crate) fn shell(script: &str, cwd: &Path) -> Command {
    let mut command = Command::new("bash");
    command
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .process_group(0)
        .kill_on_drop(true);
    command
}

/// Kills the command's whole group when dropped while armed: a bash call
/// dropped mid-run, as when a front-end quits during a turn, or a monitor
/// that ends any way but by its command exiting. `kill_on_drop` alone
/// reaches only bash, not what it started. Both tools disarm it (set it to
/// `None`) once the command has exited and been reaped, so processes it
/// put in the background live on, and so the kill never names a pid that
/// could have been reused.
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
