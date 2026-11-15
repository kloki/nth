//! `nth notify`: one sample notification through the configured backend,
//! to see what they look like and that they arrive.

use std::{io::Write, time::Duration};

use anyhow::{Result, anyhow};
use clap::ValueEnum;
use nth_config::Config;
use nth_icons::icons;
use nth_notify::{Context, Event, Notification};
use owo_colors::OwoColorize;

use crate::output::{self, Report};

#[derive(Clone, Copy, ValueEnum)]
pub enum Sample {
    Done,
    PlanReady,
    Failed,
    NeedsYou,
}

impl Sample {
    fn event(self) -> Event {
        match self {
            Sample::Done => Event::Done,
            Sample::PlanReady => Event::PlanReady,
            Sample::Failed => Event::Failed("sample error from `nth notify`".into()),
            Sample::NeedsYou => Event::NeedsYou {
                header: "Sample".into(),
                question: "Is this notification readable?".into(),
                more: 1,
            },
        }
    }

    /// The notification this sample sends, as a finished turn titled
    /// `nth notify` would raise it.
    fn notification(self) -> Notification {
        let cx = Context {
            title: "nth notify".into(),
            elapsed: Duration::from_secs(83),
            reply: Some("A sample of what a finished turn looks like.".into()),
        };
        self.event().notification(&cx)
    }
}

pub async fn run(sample: Sample, json: bool, config: &Config) -> Result<()> {
    let backend = config
        .notify
        .backend()
        .ok_or_else(|| anyhow!("notifications are off: [notify] enabled = false"))?;
    let notification = sample.notification();
    let sent = backend.send(&notification).await;
    let report = SentReport {
        backend: backend.name(),
        summary: notification.summary,
        error: sent.err().map(|e| e.to_string()),
    };
    output::print(&report, json)?;
    report.exit()
}

/// Whether the sample notification went out, through which backend.
struct SentReport {
    backend: &'static str,
    summary: String,
    /// Why the backend could not send it.
    error: Option<String>,
}

impl SentReport {
    /// Fails once the report is printed when the send did, so the command
    /// exits non-zero with the reason as its message.
    fn exit(&self) -> Result<()> {
        match &self.error {
            Some(error) => Err(anyhow!("{}: {error}", self.backend)),
            None => Ok(()),
        }
    }
}

impl Report for SentReport {
    /// Nothing on a failed send: the error `exit` returns says it.
    fn text(&self, out: &mut dyn Write) -> Result<()> {
        if self.error.is_none() {
            writeln!(
                out,
                "{} sent via {}  {}",
                icons().ok.green().bold(),
                self.backend.cyan(),
                self.summary.dimmed()
            )?;
        }
        Ok(())
    }

    fn json(&self, out: &mut dyn Write) -> Result<()> {
        let line = match &self.error {
            None => serde_json::json!({ "sent": true, "backend": self.backend }),
            Some(error) => serde_json::json!({
                "sent": false,
                "backend": self.backend,
                "error": error,
            }),
        };
        writeln!(out, "{line}")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::output::testing;

    fn report(error: Option<&str>) -> SentReport {
        SentReport {
            backend: "notify-send",
            summary: "nth notify: done".into(),
            error: error.map(Into::into),
        }
    }

    fn line(report: &SentReport) -> Value {
        serde_json::from_str(&testing::json(report)).expect("one JSON line")
    }

    #[test]
    fn a_sent_notification_names_its_backend_and_summary() {
        insta::assert_snapshot!(testing::text(&report(None)));
    }

    #[test]
    fn a_failed_send_prints_no_text_and_fails_with_the_reason() {
        let report = report(Some("no daemon"));

        assert_eq!(testing::text(&report), "");
        let error = report.exit().expect_err("the send failed");
        assert_eq!(error.to_string(), "notify-send: no daemon");
    }

    #[test]
    fn a_sent_notification_is_a_json_line_and_exits_cleanly() {
        let report = report(None);
        let line = line(&report);

        assert_eq!(line["sent"], true);
        assert_eq!(line["backend"], "notify-send");
        assert!(line.get("error").is_none(), "{line}");
        assert!(report.exit().is_ok());
        insta::assert_json_snapshot!(line);
    }

    #[test]
    fn every_sample_renders_a_notification() {
        for sample in Sample::value_variants() {
            let notification = sample.notification();

            assert!(!notification.summary.is_empty());
            assert!(!notification.body.is_empty(), "{}", notification.summary);
        }
    }

    #[test]
    fn a_failed_send_is_a_json_line_with_the_error() {
        let line = line(&report(Some("no daemon")));

        assert_eq!(line["sent"], false);
        assert_eq!(line["error"], "no daemon");
    }
}
