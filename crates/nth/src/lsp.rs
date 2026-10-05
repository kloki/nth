//! `nth lsp`: which language servers apply here, and what they say about a
//! file. The same touch the write tool does, from the terminal.

use std::{
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use clap::Subcommand;
use nth_lsp::{Lsp, ServerState, report};
use owo_colors::OwoColorize;

use crate::config::Config;

#[derive(Subcommand)]
pub enum LspCommand {
    /// Start the file's language servers and print the errors they report
    Diagnostics { file: PathBuf },
}

pub async fn run(command: Option<LspCommand>, json: bool, config: &Config) -> Result<()> {
    let lsp = Lsp::new(&config.lsp);
    match command {
        None => servers(lsp, json).await,
        Some(LspCommand::Diagnostics { file }) => diagnostics(lsp, &file, json).await,
    }
}

/// Every server, whether its program is on PATH, and the root it would run
/// in for a file in the working directory.
async fn servers(lsp: Lsp, json: bool) -> Result<()> {
    let cwd = std::env::current_dir().context("no working directory")?;
    // Finding programs and roots walks PATH and the directory tree.
    let servers = tokio::task::spawn_blocking(move || lsp.servers_for(&cwd))
        .await
        .context("server lookup failed")?;

    let mut out = std::io::stdout().lock();
    if json || !out.is_terminal() {
        for server in &servers {
            let line = serde_json::json!({
                "id": server.id,
                "extensions": server.extensions,
                "program": server.program,
                "root": server.root,
            });
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }

    let width = servers.iter().map(|s| s.id.len()).max().unwrap_or(0);
    for server in &servers {
        match &server.program {
            Some(program) => {
                let root = match &server.root {
                    Some(root) => format!("→ {}", root.display()),
                    None => "no project root here".into(),
                };
                writeln!(
                    out,
                    "{} {:width$}  {}  {}",
                    "✓".green().bold(),
                    server.id.cyan(),
                    program.display().dimmed(),
                    root.dimmed()
                )?
            }
            None => writeln!(
                out,
                "{} {:width$}  {}",
                "✗".red(),
                server.id.dimmed(),
                "not on PATH".dimmed()
            )?,
        }
    }
    Ok(())
}

/// What the write tool would append after writing `file`.
async fn diagnostics(lsp: Lsp, file: &Path, json: bool) -> Result<()> {
    let file = std::path::absolute(file).context("no working directory")?;
    anyhow::ensure!(file.is_file(), "{} is not a file", file.display());
    let diagnostics = lsp.touch(&file, true).await;
    lsp.shutdown().await;

    let mut out = std::io::stdout().lock();
    if json || !out.is_terminal() {
        for (path, diagnostics) in &diagnostics {
            for diagnostic in diagnostics {
                let mut line = serde_json::to_value(diagnostic)?;
                line["file"] = serde_json::json!(path);
                writeln!(out, "{line}")?;
            }
        }
        return Ok(());
    }

    let status = lsp.status().borrow().clone();
    if status.is_empty() {
        writeln!(
            out,
            "{} {}",
            "→".cyan().bold(),
            "no language server for this file".dimmed()
        )?;
        return Ok(());
    }
    for server in &status {
        if let ServerState::Broken(reason) = &server.state {
            writeln!(out, "{} {} {}", "✗".red(), server.id, reason.red())?;
        }
    }
    let text = report::after_write(&file, &diagnostics);
    match text.trim_start_matches('\n') {
        "" => writeln!(out, "{} {}", "✓".green().bold(), "no errors".dimmed())?,
        text => writeln!(out, "{text}")?,
    }
    Ok(())
}
