//! `nth models`: the models the providers serve, with their limits and
//! efforts and the one in use marked, and on stderr the providers that
//! could not be asked.

use std::io::{IsTerminal, Write};

use anyhow::{Result, anyhow};
use nth_protocol::Provider;
use owo_colors::OwoColorize;

use crate::{config::Config, providers};

pub async fn run(json: bool, config: Config) -> Result<()> {
    let providers = providers(&config)?;
    let listing = providers.models().await.map_err(|e| anyhow!(e))?;
    for failed in &listing.failed {
        eprintln!(
            "{} {}: {}",
            "✗".red().bold(),
            failed.origin.name.bold(),
            failed.error
        );
    }
    let models = listing.models;

    let mut out = std::io::stdout().lock();
    if json || !out.is_terminal() {
        for model in &models {
            writeln!(out, "{}", serde_json::to_string(model)?)?;
        }
        return Ok(());
    }

    // One group per provider, its name above its models.
    let current = providers.qualify(&config.model);
    let width = models.iter().map(|m| m.wire_id().len()).max().unwrap_or(0);
    let mut shown: Option<&str> = None;
    for model in &models {
        let origin = model.origin.as_ref().map(|o| o.name.as_str());
        if origin.is_some() && origin != shown {
            writeln!(out, "{}", origin.unwrap_or_default().bold())?;
            shown = origin;
        }
        let efforts: Vec<&str> = model.efforts.iter().map(|e| e.name()).collect();
        let limits = [model.limits(), efforts.join("/")]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ")
            .dimmed()
            .to_string();
        let id = model.wire_id();
        if model.id == current {
            writeln!(out, "{} {:width$}  {limits}", "→".cyan().bold(), id.bold())?;
        } else {
            writeln!(out, "  {id:width$}  {limits}")?;
        }
    }
    Ok(())
}
