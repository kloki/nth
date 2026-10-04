//! Running shell commands so that everything they start can be killed:
//! shared by bash, which waits for its command, and monitor, which leaves
//! its command running.

use std::{path::Path, process::Stdio};

use tokio::process::{Child, Command};

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

/// Kills the command's whole group if the call is dropped mid-run, as when
/// a front-end quits during a turn. `kill_on_drop` alone reaches only bash,
/// not what it started.
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
    // pid of our own child, which is still unreaped and so cannot be reused.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}
