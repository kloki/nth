//! macOS Notification Center, through `osascript`.

use std::process::Stdio;

use futures::future::BoxFuture;
use tokio::process::Command;

use crate::{
    Backend, BoxError, Notification,
    command::{self, SEND_TIMEOUT},
};

pub struct Osascript;

impl Backend for Osascript {
    fn name(&self) -> &'static str {
        "osascript"
    }

    fn send<'a>(&'a self, notification: &'a Notification) -> BoxFuture<'a, Result<(), BoxError>> {
        let mut send = Command::new("osascript");
        send.arg("-e")
            .arg(script(notification))
            .stdin(Stdio::null());
        Box::pin(command::run(send, SEND_TIMEOUT))
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
