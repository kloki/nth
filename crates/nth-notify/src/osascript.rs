//! macOS Notification Center, through `osascript`.

use std::process::Stdio;

use futures::future::BoxFuture;
use tokio::process::Command;

use crate::{Backend, BoxError, Notification};

pub struct Osascript;

impl Backend for Osascript {
    fn name(&self) -> &'static str {
        "osascript"
    }

    fn send<'a>(&'a self, notification: &'a Notification) -> BoxFuture<'a, Result<(), BoxError>> {
        Box::pin(async move {
            let output = Command::new("osascript")
                .arg("-e")
                .arg(script(notification))
                .stdin(Stdio::null())
                .output()
                .await?;
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

fn script(notification: &Notification) -> String {
    format!(
        "display notification {} with title \"nth\" subtitle {}",
        quote(&notification.body),
        quote(&notification.summary)
    )
}

/// An AppleScript string literal, so a model's quotes cannot end it early.
fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_and_backslashes_stay_inside_the_strings() {
        let notification = Notification {
            summary: "done".into(),
            body: r#"say "hi" \ bye"#.into(),
        };
        assert_eq!(
            script(&notification),
            r#"display notification "say \"hi\" \\ bye" with title "nth" subtitle "done""#
        );
    }
}
