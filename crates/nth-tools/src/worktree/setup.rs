//! The project's `.nth/worktree-setup`, run in a worktree when it is
//! created: installing dependencies, copying untracked config, whatever a
//! fresh checkout needs before it builds.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use nth_directories::{Directories, NTH};

use crate::{
    output,
    process::{self, Outcome, Tail},
};

/// Where the script lives in the main checkout `root`.
pub(super) fn script(root: &Path) -> PathBuf {
    Directories::from_root(root).worktree_setup()
}

/// Long enough for a dependency install, short enough that a script
/// waiting for input does not hold the turn forever.
const TIMEOUT: Duration = Duration::from_secs(600);

/// Runs `<root>/.nth/worktree-setup` with bash in `worktree`, if the
/// project has one, and says how it went for the tool result.
pub(super) async fn run(root: &Path, worktree: &Path, session: &str) -> Option<String> {
    run_for(root, worktree, session, TIMEOUT).await
}

async fn run_for(root: &Path, worktree: &Path, session: &str, timeout: Duration) -> Option<String> {
    let path = script(root);
    if !path.is_file() {
        return None;
    }
    // The result names it relative to the project, as the user wrote it.
    let name = Path::new(NTH).join("worktree-setup");
    let name = name.display();
    // Run through bash so the script needs no executable bit, and named
    // through the variable so no path needs quoting.
    let command = format!("exec 2>&1\nexec bash \"$NTH_WORKTREE_BASE/{name}\"");
    let spawned = process::shell(&command, worktree, session)
        .env("NTH_WORKTREE_BASE", root)
        .env("NTH_WORKTREE_PATH", worktree)
        .stdout(Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => return Some(format!("Could not run {name}: {e}")),
    };
    let Some(mut stdout) = child.stdout.take() else {
        return Some(format!("Could not run {name}: its stdout is not piped"));
    };
    let mut ring = Tail::new(output::MAX_CHARS);
    let outcome = process::run_to_exit(&mut child, &mut stdout, Some(timeout), &mut ring).await;
    let outcome = match outcome {
        Outcome::Exited(status) => match status.code() {
            Some(code) => format!("exit {code}"),
            None => "killed by signal".to_string(),
        },
        Outcome::TimedOut => format!("timed out after {timeout:?} and was killed"),
        Outcome::Failed(e) => format!("failed: {e}"),
    };
    let out = ring.text();
    Some(format!("Ran {name} ({outcome}):\n{}", out.trim_end()))
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
            script(root),
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
    async fn strips_setup_output() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".nth")).unwrap();
        std::fs::write(script(dir.path()), "printf '\\033[31mhello\\033[0m\\n'").unwrap();
        let out = run_for(dir.path(), dir.path(), "", Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(out, "Ran .nth/worktree-setup (exit 0):\nhello");
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
