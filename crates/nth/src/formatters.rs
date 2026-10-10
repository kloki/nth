//! `nth formatters`: every formatter nth knows, and whether it applies to
//! the working directory.

use std::io::{IsTerminal, Write};

use anyhow::{Context, Result};
use nth_icons::icons;
use owo_colors::OwoColorize;

use crate::config::Config;

pub async fn run(json: bool, config: &Config) -> Result<()> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let status = nth_format::Formatters::new(&config.format)
        .status(&cwd)
        .await;

    let mut out = std::io::stdout().lock();
    if json || !out.is_terminal() {
        for formatter in &status {
            let line = match &formatter.command {
                Ok(command) => serde_json::json!({
                    "name": formatter.name,
                    "extensions": formatter.extensions,
                    "enabled": true,
                    "command": command,
                }),
                Err(reason) => serde_json::json!({
                    "name": formatter.name,
                    "extensions": formatter.extensions,
                    "enabled": false,
                    "reason": reason,
                }),
            };
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }

    let width = status.iter().map(|f| f.name.len()).max().unwrap_or(0);
    for formatter in &status {
        match &formatter.command {
            Ok(command) => writeln!(
                out,
                "{} {:width$}  {}",
                icons().ok.green().bold(),
                formatter.name.cyan(),
                command.join(" ").dimmed()
            )?,
            Err(reason) => writeln!(
                out,
                "{} {:width$}  {}",
                icons().fail.red(),
                formatter.name.dimmed(),
                reason.dimmed()
            )?,
        }
    }
    Ok(())
}
