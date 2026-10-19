//! `nth memory`: what the model saved about the project in the working
//! directory, by kind, and how much of the index the system prompt takes.

use std::io::{IsTerminal, Write};

use anyhow::{Context, Result};
use nth_context::memory::{self, Entry, INDEX, INDEX_BYTES, INDEX_LINES, Kind};
use nth_icons::icons;
use owo_colors::OwoColorize;

use crate::{config::Config, text::shorten};

pub fn run(json: bool, config: &Config) -> Result<()> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let dir = memory::dir(&cwd);
    let mut warnings = Vec::new();
    let entries = memory::list(&dir, &mut warnings);
    let index = std::fs::read_to_string(dir.join(INDEX)).ok();

    let mut out = std::io::stdout().lock();
    if json || !out.is_terminal() {
        for entry in &entries {
            let line = serde_json::json!({
                "name": entry.name,
                "type": entry.kind.name(),
                "description": entry.description,
                "path": entry.path,
            });
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }

    writeln!(out, "{}  {}", "memory".bold(), dir.display().dimmed())?;
    if !config.memory.enabled {
        writeln!(
            out,
            "{} {}",
            icons().warn.yellow().bold(),
            "off: [memory] enabled = false, so no session reads or writes it".yellow()
        )?;
    }
    for warning in &warnings {
        writeln!(out, "{} {warning}", icons().warn.yellow().bold())?;
    }
    match &index {
        Some(index) => {
            let lines = index.lines().count();
            let over = lines > INDEX_LINES || index.len() > INDEX_BYTES;
            let size = format!(
                "{INDEX}  {lines}/{INDEX_LINES} lines · {}/{} KB",
                index.len().div_ceil(1000),
                INDEX_BYTES / 1000
            );
            match over {
                true => writeln!(
                    out,
                    "{} {}",
                    icons().warn.yellow().bold(),
                    format!("{size}, only the first part loads").yellow()
                )?,
                false => writeln!(out, "{}", size.dimmed())?,
            }
        }
        None => writeln!(out, "{}", format!("no {INDEX} yet").dimmed())?,
    }
    if entries.is_empty() {
        writeln!(
            out,
            "{} {}",
            icons().to.cyan().bold(),
            "no memories saved".dimmed()
        )?;
        return Ok(());
    }
    let width = entries.iter().map(|e| e.name.len()).max().unwrap_or(0);
    for kind in Kind::ALL {
        let group: Vec<&Entry> = entries.iter().filter(|e| e.kind == kind).collect();
        if group.is_empty() {
            continue;
        }
        writeln!(out, "{}", kind.name().bold())?;
        let tree = &icons().tree;
        for (i, entry) in group.iter().enumerate() {
            let branch = match i + 1 == group.len() {
                true => tree.elbow,
                false => tree.tee,
            };
            writeln!(
                out,
                "{branch}{} {:width$}  {}",
                tree.dash,
                entry.name.cyan(),
                shorten(&entry.description).dimmed()
            )?;
        }
    }
    Ok(())
}
