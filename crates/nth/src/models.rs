//! `nth models`: the models the endpoint serves, with the one in use marked.

use std::io::{IsTerminal, Write};

use anyhow::{Result, anyhow};
use nth_protocol::Provider;
use owo_colors::OwoColorize;

use crate::{client, config::Config};

pub async fn run(json: bool, config: Config) -> Result<()> {
    let provider = client(&config)?;
    let models = provider.models().await.map_err(|e| anyhow!(e))?;

    let mut out = std::io::stdout().lock();
    if json || !out.is_terminal() {
        for model in &models {
            writeln!(out, "{}", serde_json::to_string(model)?)?;
        }
        return Ok(());
    }

    let width = models.iter().map(|m| m.id.len()).max().unwrap_or(0);
    for model in &models {
        let limits = model.limits().dimmed().to_string();
        if model.id == config.provider.model {
            writeln!(
                out,
                "{} {:width$}  {limits}",
                "→".cyan().bold(),
                model.id.bold()
            )?;
        } else {
            writeln!(out, "  {:width$}  {limits}", model.id)?;
        }
    }
    Ok(())
}
