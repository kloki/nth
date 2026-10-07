//! The freedesktop notification daemon, through `notify-send` (libnotify).

use std::{io::ErrorKind, process::Stdio};

use futures::future::BoxFuture;
use tokio::process::Command;

use crate::{Backend, BoxError, Notification, Urgency};

pub struct NotifySend;

impl Backend for NotifySend {
    fn name(&self) -> &'static str {
        "notify-send"
    }

    fn send<'a>(&'a self, notification: &'a Notification) -> BoxFuture<'a, Result<(), BoxError>> {
        Box::pin(async move {
            let output = Command::new("notify-send")
                .args(args(notification))
                .stdin(Stdio::null())
                .output()
                .await
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
        })
    }
}

fn args(notification: &Notification) -> Vec<String> {
    let urgency = match notification.urgency {
        Urgency::Normal => "normal",
        Urgency::Critical => "critical",
    };
    vec![
        "--app-name=nth".into(),
        format!("--urgency={urgency}"),
        // Ends the options, so a summary starting with `-` stays text.
        "--".into(),
        notification.summary.clone(),
        escape(&notification.body),
    ]
}

/// Daemons read the body as markup, where a model's `<T>` or `&&` would
/// break it or vanish.
fn escape(body: &str) -> String {
    body.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_markup_is_escaped_and_options_end_before_the_text() {
        let notification = Notification {
            summary: "-done".into(),
            body: "Vec<T> && more".into(),
            urgency: Urgency::Critical,
        };
        assert_eq!(
            args(&notification),
            [
                "--app-name=nth",
                "--urgency=critical",
                "--",
                "-done",
                "Vec&lt;T&gt; &amp;&amp; more"
            ]
        );
    }
}
