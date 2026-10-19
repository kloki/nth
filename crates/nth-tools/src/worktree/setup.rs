//! The project's `.nth/worktree-setup`, run in a worktree when it is
//! created: installing dependencies, copying untracked config, whatever a
//! fresh checkout needs before it builds.

use std::{path::Path, process::Stdio, time::Duration};

use tokio::io::AsyncReadExt;

use crate::{
    output::{self, tail},
    process::{self, DRAIN, KillGroupOnDrop, kill_group},
};

/// Where the script lives, relative to the main checkout.
pub(super) const SCRIPT: &str = ".nth/worktree-setup";

/// Long enough for a dependency install, short enough that a script
/// waiting for input does not hold the turn forever.
const TIMEOUT: Duration = Duration::from_secs(600);

/// Runs `<root>/.nth/worktree-setup` with bash in `worktree`, if the
/// project has one, and says how it went for the tool result.
pub(super) async fn run(root: &Path, worktree: &Path, session: &str) -> Option<String> {
    run_for(root, worktree, session, TIMEOUT).await
}

async fn run_for(root: &Path, worktree: &Path, session: &str, timeout: Duration) -> Option<String> {
    if !root.join(SCRIPT).is_file() {
        return None;
    }
    // Run through bash so the script needs no executable bit, and named
    // through the variable so no path needs quoting.
    let script = format!("exec 2>&1\nexec bash \"$NTH_WORKTREE_BASE/{SCRIPT}\"");
    let spawned = process::shell(&script, worktree, session)
        .env("NTH_WORKTREE_BASE", root)
        .env("NTH_WORKTREE_PATH", worktree)
        .stdout(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => return Some(format!("Could not run {SCRIPT}: {e}")),
    };
    let Some(mut stdout) = child.stdout.take() else {
        return Some(format!("Could not run {SCRIPT}: its stdout is not piped"));
    };
    // Declared after `child` so it drops first, while the child is still
    // unreaped: a cancelled turn kills the whole script.
    let mut abandoned = KillGroupOnDrop(child.id());

    let mut buf = Vec::new();
    let exit = async {
        loop {
            tokio::select! {
                read = stdout.read_buf(&mut buf) => {
                    if read? == 0 {
                        return child.wait().await;
                    }
                }
                status = child.wait() => return status,
            }
        }
    };
    let run = tokio::time::timeout(timeout, exit).await;
    let outcome = match run {
        Ok(Ok(status)) => match status.code() {
            Some(code) => format!("exit {code}"),
            None => "killed by signal".to_string(),
        },
        Ok(Err(e)) => format!("failed: {e}"),
        Err(_) => {
            kill_group(&child);
            format!("timed out after {timeout:?} and was killed")
        }
    };
    abandoned.0 = None;
    // Processes the script put in the background may hold the pipe open;
    // take what they already wrote and move on.
    let _ = tokio::time::timeout(DRAIN, stdout.read_to_end(&mut buf)).await;
    let out = tail(&String::from_utf8_lossy(&buf), output::MAX_CHARS);
    Some(format!("Ran {SCRIPT} ({outcome}):\n{}", out.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A script that prints, then waits on a process of its own that
    /// leaves a mark a second later if it is still alive: killing only the
    /// script, as `kill_on_drop` does, would not stop it.
    fn hanging(root: &Path) {
        std::fs::create_dir_all(root.join(".nth")).expect("mkdir");
        std::fs::write(
            root.join(SCRIPT),
            "echo started\nbash -c 'sleep 1; touch \"$NTH_WORKTREE_PATH/survived\"'\n",
        )
        .expect("write");
    }

    async fn survived(root: &Path) -> bool {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        root.join("survived").exists()
    }

    #[tokio::test]
    async fn a_hanging_script_is_killed_with_its_output_so_far() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        hanging(root);

        let out = run_for(root, root, "", Duration::from_millis(300))
            .await
            .expect("ran");

        assert_eq!(
            out,
            "Ran .nth/worktree-setup (timed out after 300ms and was killed):\nstarted"
        );
        assert!(!survived(root).await, "the script outlived its timeout");
    }

    #[tokio::test]
    async fn a_cancelled_run_kills_the_script() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        hanging(root);

        let cancelled = tokio::time::timeout(
            Duration::from_millis(300),
            run_for(root, root, "", Duration::from_secs(60)),
        )
        .await;

        assert!(cancelled.is_err(), "{cancelled:?}");
        assert!(!survived(root).await, "the script outlived its turn");
    }
}
