//! Running shell commands so that everything they start can be killed:
//! shared by bash and the worktree setup script, which wait for their
//! command, and monitor, which leaves its command running.

use std::{path::Path, process::Stdio, time::Duration};

use tokio::process::{Child, Command};

/// The shortest timeout bash and monitor accept from the model. A `0` is
/// a mistake, not a wish to kill the command before it starts.
pub(crate) const MIN_TIMEOUT_MS: u64 = 1_000;

/// How long to keep reading after the command exits, for output still in
/// its pipes.
pub(crate) const DRAIN: Duration = Duration::from_millis(100);

pub(crate) fn reject_old_timeout(args: &serde_json::Value) -> Result<(), String> {
    for key in ["timeout", "timeout_secs"] {
        if args.get(key).is_some() {
            return Err(format!("obsolete argument {key}; use timeout_ms"));
        }
    }
    Ok(())
}

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

#[cfg(test)]
mod tests {
    use super::*;

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
        use nth_protocol::{Tool, ToolContext};
        use serde_json::json;
        let ctx = ToolContext::new(Path::new(".").to_path_buf());
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(crate::bash::Bash::default()),
            Box::new(crate::webfetch::WebFetch),
            Box::new(crate::monitor::Monitor),
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
