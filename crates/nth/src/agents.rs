//! `nth agents`: the agents the model can delegate to for the working
//! directory, grouped by where they came from.

use std::io::{IsTerminal, Write};

use anyhow::{Context, Result};
use owo_colors::OwoColorize;

use crate::{config::Config, context};

/// Longest description shown in the terminal listing.
const DESCRIPTION_CHARS: usize = 72;

pub async fn run(json: bool, config: &Config) -> Result<()> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let context = context(cwd, &config.paths()).await;

    let mut out = std::io::stdout().lock();
    if json || !out.is_terminal() {
        for agent in context.agents.iter() {
            let line = serde_json::json!({
                "name": agent.name,
                "description": agent.description,
                "model": agent.model,
                "tools": agent.tools,
                "path": agent.path,
                "source": agent.source.name(),
            });
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }

    let mut sources: Vec<_> = context.agents.iter().map(|a| a.source).collect();
    sources.sort();
    sources.dedup();
    let width = context
        .agents
        .iter()
        .map(|a| a.name.len())
        .max()
        .unwrap_or(0);
    for source in sources {
        writeln!(out, "{}", source.name().bold())?;
        let group: Vec<_> = context
            .agents
            .iter()
            .filter(|a| a.source == source)
            .collect();
        for (i, agent) in group.iter().enumerate() {
            let branch = if i + 1 == group.len() {
                "└─"
            } else {
                "├─"
            };
            let about = match &agent.description {
                Some(d) => shorten(d).dimmed().to_string(),
                None => "no description, so the model is not offered it"
                    .yellow()
                    .to_string(),
            };
            writeln!(
                out,
                "{} {:width$}  {about}",
                branch.dimmed(),
                agent.name.cyan()
            )?;
        }
    }
    Ok(())
}

/// The first line of `text`, cut to fit one terminal row.
fn shorten(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(DESCRIPTION_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}
