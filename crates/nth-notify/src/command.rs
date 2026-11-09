//! Running a backend's command, which both backends share.

use std::{io::ErrorKind, time::Duration};

use tokio::process::Command;

use crate::BoxError;

/// How long a backend's command gets. A notification daemon that stopped
/// answering would otherwise hold its `notify-send` open for good, and the
/// queue behind it.
pub(crate) const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs `command`, killed if it is still running after `timeout`; its stderr
/// is the error when it fails.
pub(crate) async fn run(mut command: Command, timeout: Duration) -> Result<(), BoxError> {
    let output = command.kill_on_drop(true).output();
    let output = tokio::time::timeout(timeout, output)
        .await
        .map_err(|_| format!("no answer within {} s", timeout.as_secs_f32()))?
        .map_err(|e| match e.kind() {
            ErrorKind::NotFound => "not found on PATH".into(),
            _ => BoxError::from(e),
        })?;
    match output.status.success() {
        true => Ok(()),
        false => Err(String::from_utf8_lossy(&output.stderr)
            .trim()
            .to_string()
            .into()),
    }
}

#[cfg(test)]
mod tests {
    use std::{process::Stdio, time::Instant};

    use super::*;

    #[tokio::test]
    async fn a_hung_notifier_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(r#"echo $$ > "$1"; exec sleep 30"#)
            .arg("sh")
            .arg(&pid_file)
            .stdin(Stdio::null());

        let started = Instant::now();
        let error = run(command, Duration::from_millis(200)).await.unwrap_err();
        assert!(error.to_string().contains("no answer within"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(5));

        // The hung command is killed, not left behind.
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let stat = format!("/proc/{}/stat", pid.trim());
        if cfg!(target_os = "linux") {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                // A zombie waiting to be reaped is dead too.
                match std::fs::read_to_string(&stat) {
                    Err(_) => break,
                    Ok(stat) if stat.rsplit(')').next().unwrap().trim().starts_with('Z') => break,
                    Ok(_) => {}
                }
                assert!(Instant::now() < deadline, "{pid} still runs");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }

    #[tokio::test]
    async fn a_failing_command_says_why() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("echo nope >&2; exit 1");
        let error = run(command, SEND_TIMEOUT).await.unwrap_err();
        assert_eq!(error.to_string(), "nope");
    }

    #[tokio::test]
    async fn a_missing_command_is_not_on_path() {
        let command = Command::new("nth-no-such-notifier");
        let error = run(command, SEND_TIMEOUT).await.unwrap_err();
        assert_eq!(error.to_string(), "not found on PATH");
    }
}
