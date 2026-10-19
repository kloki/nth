//! `nth agents`: the agents the model can delegate to for the working
//! directory, grouped by where they came from.

use std::{
    collections::BTreeMap,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use nth_context::agents::{Agent, Agents, Source};
use owo_colors::OwoColorize;

use crate::{config::Config, context, text::shorten};

pub async fn run(json: bool, config: &Config) -> Result<()> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let paths = config.paths();
    let context = context(cwd, &paths).await;

    let mut out = std::io::stdout().lock();
    if json || !out.is_terminal() {
        for agent in context.agents.iter() {
            let line = serde_json::json!({
                "name": agent.name,
                "description": agent.description,
                "model": agent.model,
                "tools": agent.tools,
                "denied": agent.denied,
                "hidden": agent.hidden,
                "path": agent.path,
                "root": agent.root,
                "source": agent.source.name(),
            });
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }

    let width = context
        .agents
        .iter()
        .map(|a| a.name.len())
        .max()
        .unwrap_or(0);
    for ((source, root), group) in groups(&context.agents) {
        let heading = match &root {
            Some(root) => format!(
                "{}  {}",
                source.name().bold(),
                place(root, paths.home.as_deref()).dimmed()
            ),
            None => source.name().bold().to_string(),
        };
        writeln!(out, "{heading}")?;
        for (i, agent) in group.iter().enumerate() {
            let branch = if i + 1 == group.len() {
                "└─"
            } else {
                "├─"
            };
            let nested = match nested_folder(agent, root.as_deref()) {
                Some(folder) => format!("  {}", folder.dimmed()),
                None => String::new(),
            };
            let about = match &agent.description {
                Some(d) => shorten(d).dimmed().to_string(),
                None => "no description, so the model is not offered it"
                    .yellow()
                    .to_string(),
            };
            writeln!(
                out,
                "{} {:width$}{nested}  {about}",
                branch.dimmed(),
                agent.name.cyan()
            )?;
        }
    }
    Ok(())
}

/// The agents by source and by the root folder they were found under, making
/// `global` granular as `nth skills` does.
fn groups(agents: &Agents) -> BTreeMap<(Source, Option<PathBuf>), Vec<&Agent>> {
    let mut groups: BTreeMap<(Source, Option<PathBuf>), Vec<&Agent>> = BTreeMap::new();
    for agent in agents.iter() {
        groups
            .entry((agent.source, agent.root.clone()))
            .or_default()
            .push(agent);
    }
    groups
}

/// The agent's folder below its root when it sits more than one level down
/// (e.g. `group/review`); a built-in has none.
fn nested_folder(agent: &Agent, root: Option<&Path>) -> Option<String> {
    let dir = agent.path.as_deref()?.parent()?;
    let root = root?;
    let folder = dir.strip_prefix(root).unwrap_or(dir);
    (folder.components().count() > 1).then(|| folder.display().to_string())
}

/// `path` with `home` written as `~`, as the status bar shows it.
fn place(path: &Path, home: Option<&Path>) -> String {
    match home {
        Some(home) if !home.as_os_str().is_empty() => match path.strip_prefix(home) {
            Ok(rest) => format!("~/{}", rest.display())
                .trim_end_matches('/')
                .to_string(),
            Err(_) => path.display().to_string(),
        },
        _ => path.display().to_string(),
    }
}
