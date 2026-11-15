//! `nth formatters`: every formatter nth knows, and whether it applies to
//! the working directory.

use std::{io::Write, path::Path};

use anyhow::{Context, Result};
use nth_config::Config;
use nth_format::{FormatterStatus, Formatters};
use nth_icons::icons;
use owo_colors::OwoColorize;

use crate::output::{self, Report};

pub async fn run(json: bool, config: &Config) -> Result<()> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let report = FormattersReport::read(&cwd, config).await;
    output::print(&report, json)
}

/// Every formatter, in the order they run, with the command it runs in
/// `cwd` or why it does not.
struct FormattersReport {
    formatters: Vec<FormatterStatus>,
}

impl FormattersReport {
    async fn read(cwd: &Path, config: &Config) -> Self {
        let formatters = Formatters::new(&config.format).status(cwd).await;
        Self { formatters }
    }
}

impl Report for FormattersReport {
    fn text(&self, out: &mut dyn Write) -> Result<()> {
        let width = self
            .formatters
            .iter()
            .map(|f| f.name.len())
            .max()
            .unwrap_or(0);
        for formatter in &self.formatters {
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

    fn json(&self, out: &mut dyn Write) -> Result<()> {
        for formatter in &self.formatters {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::output::testing;

    /// One formatter that runs and one that does not, their names of
    /// different lengths so the column shows.
    fn report() -> FormattersReport {
        FormattersReport {
            formatters: vec![
                FormatterStatus {
                    name: "rustfmt".into(),
                    extensions: vec![".rs".into()],
                    command: Ok(vec!["rustfmt".into(), "$FILE".into()]),
                },
                FormatterStatus {
                    name: "gofmt".into(),
                    extensions: vec![".go".into()],
                    command: Err("gofmt not found".into()),
                },
            ],
        }
    }

    fn lines(report: &FormattersReport) -> Vec<Value> {
        testing::json(report)
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON line"))
            .collect()
    }

    #[test]
    fn an_enabled_formatter_shows_its_command_and_a_disabled_one_why() {
        insta::assert_snapshot!(testing::text(&report()));
    }

    #[test]
    fn each_formatter_is_a_json_line_saying_whether_it_is_enabled() {
        let lines = lines(&report());

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["enabled"], true);
        assert_eq!(lines[0]["command"], serde_json::json!(["rustfmt", "$FILE"]));
        assert!(lines[0].get("reason").is_none(), "{}", lines[0]);
        assert_eq!(lines[1]["enabled"], false);
        assert_eq!(lines[1]["reason"], "gofmt not found");
        assert!(lines[1].get("command").is_none(), "{}", lines[1]);
        insta::assert_json_snapshot!(lines[0]);
    }

    #[tokio::test]
    async fn read_lists_the_configs_own_formatters_after_the_built_in_ones() {
        // The built-in table and PATH decide the rest, so only the
        // config's own entries are pinned.
        let dir = tempfile::tempdir().expect("a temp dir");
        let config: Config = toml::from_str(
            "[format.mine]\ncommand = ['mine', '$FILE']\nextensions = ['.x']\n\
             [format.theirs]\ndisabled = true\n",
        )
        .expect("a config");

        let report = FormattersReport::read(dir.path(), &config).await;

        let mine = report
            .formatters
            .iter()
            .find(|f| f.name == "mine")
            .expect("the config's formatter");
        assert_eq!(mine.command, Ok(vec!["mine".into(), "$FILE".into()]));
        assert_eq!(mine.extensions, vec![".x".to_string()]);
        let theirs = report
            .formatters
            .iter()
            .find(|f| f.name == "theirs")
            .expect("the disabled one");
        assert_eq!(theirs.command, Err("disabled in the config".into()));
    }
}
