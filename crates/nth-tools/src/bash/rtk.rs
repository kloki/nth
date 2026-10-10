//! [rtk](https://github.com/rtk-ai/rtk): rewrites a command into one whose
//! output is compressed for the model (`git status` to `rtk git status`), as
//! its opencode plugin does. The rules live in rtk; nth only asks.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

/// A rewrite is a table lookup; anything slower is a broken rtk.
const TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq)]
pub struct Rtk {
    program: PathBuf,
}

impl Rtk {
    /// The `rtk` on PATH, if any. Blocking.
    pub fn find() -> Option<Self> {
        nth_context::which("rtk").map(|program| Self { program })
    }

    /// The command rtk would run instead, or `None` when it has no rule for
    /// it or fails in any way: rtk must never stop a command from running.
    pub async fn rewrite(&self, command: &str, cwd: &Path) -> Option<String> {
        let output = tokio::process::Command::new(&self.program)
            .arg("rewrite")
            .arg(command)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output();
        let output = tokio::time::timeout(TIMEOUT, output).await.ok()?.ok()?;
        // rtk's own hooks take 3 as a rewrite too.
        if !matches!(output.status.code(), Some(0 | 3)) {
            return None;
        }
        let rewritten = String::from_utf8(output.stdout).ok()?;
        let rewritten = rewritten.trim();
        (!rewritten.is_empty() && rewritten != command).then(|| rewritten.to_string())
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// An rtk that runs `script` (with the command as `$2`), written into
    /// `dir`.
    pub fn fake(dir: &Path, script: &str) -> Rtk {
        let program = dir.join("rtk");
        std::fs::write(&program, format!("#!/bin/sh\n{script}\n")).expect("write fake rtk");
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755))
            .expect("make fake rtk executable");
        Rtk { program }
    }

    async fn rewrite(script: &str, command: &str) -> Option<String> {
        let dir = tempfile::tempdir().expect("tempdir");
        fake(dir.path(), script).rewrite(command, dir.path()).await
    }

    #[tokio::test]
    async fn returns_the_rewritten_command() {
        let out = rewrite(r#"echo "rtk $2""#, "git status").await;
        assert_eq!(out.as_deref(), Some("rtk git status"));
    }

    #[tokio::test]
    async fn takes_exit_code_3_as_a_rewrite() {
        let out = rewrite(r#"echo "rtk $2"; exit 3"#, "git status").await;
        assert_eq!(out.as_deref(), Some("rtk git status"));
    }

    #[tokio::test]
    async fn the_same_command_is_no_rewrite() {
        assert_eq!(rewrite(r#"echo "$2""#, "ls").await, None);
    }

    #[tokio::test]
    async fn empty_output_is_no_rewrite() {
        assert_eq!(rewrite("true", "ls").await, None);
    }

    #[tokio::test]
    async fn a_failing_rtk_is_no_rewrite() {
        assert_eq!(rewrite(r#"echo "rtk $2"; exit 1"#, "ls").await, None);
    }

    #[tokio::test]
    async fn a_hanging_rtk_is_no_rewrite() {
        let started = std::time::Instant::now();
        assert_eq!(rewrite("sleep 30", "ls").await, None);
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_missing_rtk_is_no_rewrite() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rtk = Rtk {
            program: dir.path().join("rtk"),
        };
        assert_eq!(rtk.rewrite("ls", dir.path()).await, None);
    }
}
