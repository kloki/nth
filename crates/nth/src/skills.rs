//! `nth skills`: the skills found for the working directory, grouped by
//! where they came from.

use std::io::{IsTerminal, Write};

use anyhow::{Context, Result};
use nth_icons::icons;
use owo_colors::OwoColorize;

use crate::{config::Config, context};

/// Longest description shown in the terminal listing.
const DESCRIPTION_CHARS: usize = 72;

pub async fn run(json: bool, config: &Config) -> Result<()> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let context = context(cwd, &config.paths()).await;

    let mut out = std::io::stdout().lock();
    if json || !out.is_terminal() {
        for skill in context.skills.iter() {
            let line = serde_json::json!({
                "name": skill.name,
                "description": skill.description,
                "path": skill.path,
                "source": skill.source.name(),
            });
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }

    if context.skills.is_empty() {
        writeln!(
            out,
            "{} {}",
            icons().to.cyan().bold(),
            "no skills found".dimmed()
        )?;
        return Ok(());
    }
    let mut sources: Vec<_> = context.skills.iter().map(|s| s.source).collect();
    sources.sort();
    sources.dedup();
    let width = context
        .skills
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(0);
    for source in sources {
        writeln!(out, "{}", source.name().bold())?;
        let group: Vec<_> = context
            .skills
            .iter()
            .filter(|s| s.source == source)
            .collect();
        for (i, skill) in group.iter().enumerate() {
            let branch = if i + 1 == group.len() {
                "└─"
            } else {
                "├─"
            };
            let about = match &skill.description {
                Some(d) => shorten(d).dimmed().to_string(),
                None => "no description, so the model is not offered it"
                    .yellow()
                    .to_string(),
            };
            writeln!(
                out,
                "{} {:width$}  {about}",
                branch.dimmed(),
                skill.name.cyan()
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
