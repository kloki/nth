//! `nth memory`: what the model saved about the project in the working
//! directory, by kind, and how much of the index the system prompt takes.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use nth_config::Config;
use nth_context::memory::{self, Entry, INDEX, INDEX_BYTES, INDEX_LINES, Kind};
use nth_icons::icons;
use owo_colors::OwoColorize;

use crate::{
    output::{self, Report},
    text::{branch, shorten},
};

pub fn run(json: bool, config: &Config) -> Result<()> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let report = MemoryReport::read(&cwd, config);
    output::print(&report, json)
}

/// The project's memory folder and what is in it.
struct MemoryReport {
    dir: PathBuf,
    /// The config's `[memory] enabled`: whether any session reads it.
    enabled: bool,
    /// Memory files that could not be read.
    warnings: Vec<String>,
    /// `MEMORY.md`, when there is one.
    index: Option<String>,
    /// By kind and then name, as `memory::list` gives them.
    entries: Vec<Entry>,
}

impl MemoryReport {
    fn read(cwd: &Path, config: &Config) -> Self {
        let dir = memory::dir(cwd);
        let mut warnings = Vec::new();
        let entries = memory::list(&dir, &mut warnings);
        let index = std::fs::read_to_string(dir.join(INDEX)).ok();
        Self {
            dir,
            enabled: config.memory.enabled,
            warnings,
            index,
            entries,
        }
    }
}

impl Report for MemoryReport {
    fn text(&self, out: &mut dyn Write) -> Result<()> {
        writeln!(out, "{}  {}", "memory".bold(), self.dir.display().dimmed())?;
        if !self.enabled {
            writeln!(
                out,
                "{} {}",
                icons().warn.yellow().bold(),
                "off: [memory] enabled = false, so no session reads or writes it".yellow()
            )?;
        }
        for warning in &self.warnings {
            writeln!(out, "{} {warning}", icons().warn.yellow().bold())?;
        }
        match &self.index {
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
        if self.entries.is_empty() {
            writeln!(
                out,
                "{} {}",
                icons().to.cyan().bold(),
                "no memories saved".dimmed()
            )?;
            return Ok(());
        }
        let width = self.entries.iter().map(|e| e.name.len()).max().unwrap_or(0);
        for kind in Kind::ALL {
            let group: Vec<&Entry> = self.entries.iter().filter(|e| e.kind == kind).collect();
            if group.is_empty() {
                continue;
            }
            writeln!(out, "{}", kind.name().bold())?;
            for (i, entry) in group.iter().enumerate() {
                writeln!(
                    out,
                    "{} {:width$}  {}",
                    branch(i, group.len()),
                    entry.name.cyan(),
                    shorten(&entry.description).dimmed()
                )?;
            }
        }
        Ok(())
    }

    fn json(&self, out: &mut dyn Write) -> Result<()> {
        for entry in &self.entries {
            let line = serde_json::json!({
                "name": entry.name,
                "type": entry.kind.name(),
                "description": entry.description,
                "path": entry.path,
            });
            writeln!(out, "{line}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::output::testing;

    const DIR: &str = "/repo/.nth/memory";

    fn entry(name: &str, kind: Kind, description: &str) -> Entry {
        Entry {
            path: Path::new(DIR).join(format!("{name}.md")),
            name: name.into(),
            description: description.into(),
            kind,
        }
    }

    /// Memory that is on, with no index and nothing saved.
    fn empty() -> MemoryReport {
        MemoryReport {
            dir: DIR.into(),
            enabled: true,
            warnings: Vec::new(),
            index: None,
            entries: Vec::new(),
        }
    }

    #[test]
    fn nothing_saved_and_no_index_says_so() {
        insta::assert_snapshot!(testing::text(&empty()));
    }

    #[test]
    fn an_index_over_its_lines_warns_that_only_the_first_part_loads() {
        let report = MemoryReport {
            index: Some("- [a](a.md) — a\n".repeat(INDEX_LINES + 1)),
            ..empty()
        };

        insta::assert_snapshot!(testing::text(&report));
    }

    #[test]
    fn memory_off_says_so_beside_a_warning_from_the_list() {
        let report = MemoryReport {
            enabled: false,
            warnings: vec![format!("skipped {DIR}/broken.md: no type")],
            index: Some("- [prs](prs.md) — stacked PRs\n".into()),
            ..empty()
        };

        insta::assert_snapshot!(testing::text(&report));
    }

    #[test]
    fn entries_are_grouped_by_kind() {
        // By kind and then name, as `memory::list` gives them.
        let report = MemoryReport {
            index: Some("- [colours](colours.md)\n- [prs](prs.md)\n".into()),
            entries: vec![
                entry(
                    "colours",
                    Kind::User,
                    "Colour names are the 16 terminal colours",
                ),
                entry("worktrees", Kind::User, "Work in worktrees"),
                entry("prs", Kind::Feedback, "Deliver as stacked PRs"),
            ],
            ..empty()
        };

        insta::assert_snapshot!(testing::text(&report));
    }

    #[test]
    fn each_entry_is_a_json_line() {
        let report = MemoryReport {
            entries: vec![
                entry("colours", Kind::User, "Colour names"),
                entry("prs", Kind::Feedback, "Stacked PRs"),
            ],
            ..empty()
        };

        let json = testing::json(&report);
        let lines: Vec<Value> = json
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["name"], "colours");
        assert_eq!(lines[0]["type"], "user");
        assert_eq!(lines[0]["path"], "/repo/.nth/memory/colours.md");
        assert_eq!(lines[1]["type"], "feedback");
        insta::assert_json_snapshot!(lines[1]);
    }

    #[test]
    fn read_finds_the_entries_and_the_index() {
        let root = tempfile::tempdir().unwrap();
        let dir = memory::dir(root.path());
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, kind: &str| {
            std::fs::write(
                dir.join(format!("{name}.md")),
                format!("---\nname: {name}\ndescription: About {name}\ntype: {kind}\n---\nbody\n"),
            )
            .unwrap();
        };
        write("prs", "feedback");
        write("colours", "user");
        std::fs::write(dir.join(INDEX), "- [prs](prs.md)\n").unwrap();
        let mut config = Config::default();
        config.memory.enabled = false;

        let report = MemoryReport::read(root.path(), &config);

        assert_eq!(report.dir, dir);
        assert!(!report.enabled);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(report.index.as_deref(), Some("- [prs](prs.md)\n"));
        let names: Vec<_> = report.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["colours", "prs"]);
    }
}
