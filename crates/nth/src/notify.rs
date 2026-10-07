//! `nth notify`: one sample notification through the configured backend,
//! to see what they look like and that they arrive.

use std::{
    io::{IsTerminal, Write},
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow};
use clap::ValueEnum;
use nth_notify::{Context, Event};
use owo_colors::OwoColorize;

use crate::config::Config;

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
}

pub async fn run(sample: Sample, json: bool, config: &Config) -> Result<()> {
    let backend = config
        .notify
        .backend()
        .ok_or_else(|| anyhow!("notifications are off: [notify] enabled = false"))?;
    let cwd = std::env::current_dir().context("no working directory")?;
    let cx = Context {
        project: Context::project(&cwd),
        title: "nth notify".into(),
        elapsed: Duration::from_secs(83),
        reply: Some("A sample of what a finished turn looks like.".into()),
    };
    let notification = sample.event().notification(&cx);
    let sent = backend.send(&notification).await;

    let mut out = std::io::stdout().lock();
    if json || !out.is_terminal() {
        let line = match &sent {
            Ok(()) => serde_json::json!({ "sent": true, "backend": backend.name() }),
            Err(e) => serde_json::json!({
                "sent": false,
                "backend": backend.name(),
                "error": e.to_string(),
            }),
        };
        writeln!(out, "{line}")?;
    } else if sent.is_ok() {
        writeln!(
            out,
            "{} sent via {}  {}",
            "✓".green().bold(),
            backend.name().cyan(),
            notification.summary.dimmed()
        )?;
    }
    sent.map_err(|e| anyhow!("{}: {e}", backend.name()))
}
