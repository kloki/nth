//! `nth skills`: the skills found for the working directory, grouped by
//! where they came from.

use std::{
    collections::BTreeMap,
    io::{IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use nth_context::skills::{Skill, Skills, Source};
use nth_icons::icons;
use owo_colors::OwoColorize;

use crate::{config::Config, context};

/// Longest description shown in the terminal listing.
const DESCRIPTION_CHARS: usize = 72;

pub async fn run(json: bool, config: &Config) -> Result<()> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let paths = config.paths();
    let context = context(cwd, &paths).await;

    let mut out = std::io::stdout().lock();
    if json || !out.is_terminal() {
        for skill in context.skills.iter() {
            let line = serde_json::json!({
                "name": skill.name,
                "description": skill.description,
                "path": skill.path(),
                "root": skill.root,
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
    let width = context
        .skills
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(0);
    for ((source, root), group) in groups(&context.skills) {
        let heading = match &root {
            Some(root) => format!(
                "{}  {}",
                source.name().bold(),
                place(root, paths.home.as_deref()).dimmed()
            ),
            None => source.name().bold().to_string(),
        };
        writeln!(out, "{heading}")?;
        for (i, skill) in group.iter().enumerate() {
            let branch = if i + 1 == group.len() {
                "└─"
            } else {
                "├─"
            };
            let nested = match nested_folder(skill, root.as_deref()) {
                Some(folder) => format!("  {}", folder.dimmed()),
                None => String::new(),
            };
            let about = match &skill.description {
                Some(d) => shorten(d).dimmed().to_string(),
                None => "no description, so the model is not offered it"
                    .yellow()
                    .to_string(),
            };
            writeln!(
                out,
                "{} {:width$}{nested}  {about}",
                branch.dimmed(),
                skill.name.cyan()
            )?;
        }
    }
    Ok(())
}

/// The skills by source and by the root folder they were found under, in a
/// stable order. The folder makes `global` granular: `~/.claude/skills`,
/// `~/.config/opencode/skills`, `~/.agents/skills` and
/// `~/.config/nth/skills` are separate groups.
fn groups(skills: &Skills) -> BTreeMap<(Source, Option<PathBuf>), Vec<&Skill>> {
    let mut groups: BTreeMap<(Source, Option<PathBuf>), Vec<&Skill>> = BTreeMap::new();
    for skill in skills.iter() {
        groups
            .entry((skill.source, skill.root.clone()))
            .or_default()
            .push(skill);
    }
    groups
}

/// The skill's folder below its root, when it sits more than one level down
/// (e.g. `synced/<id>/diagram`), so `global` alone does not hide where it
/// came from. A skill directly under its root needs no folder: its name is
/// the folder.
fn nested_folder(skill: &Skill, root: Option<&Path>) -> Option<String> {
    let dir = skill.dir()?;
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

/// The first line of `text`, cut to fit one terminal row.
fn shorten(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(DESCRIPTION_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}
