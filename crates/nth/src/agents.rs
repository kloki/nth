//! `nth agents`: the agents for the working directory, those the model can
//! delegate to and those you can run as, grouped by where they came from.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use nth_context::{
    Paths,
    agents::{Agent, Mode},
};
use owo_colors::OwoColorize;

use crate::{
    context,
    output::{self, Report},
    text::{branch, groups, nested_folder, place, shorten},
};

/// `paths` are the session's global directories with the configured skill
/// folders, as `Runtime::paths` gives them.
pub async fn run(json: bool, paths: Paths) -> Result<()> {
    let cwd = std::env::current_dir().context("no working directory")?;
    let report = AgentsReport::gather(&cwd, &paths).await;
    output::print(&report, json)
}

/// The agents found for a directory, and the home the listing writes as `~`.
struct AgentsReport {
    /// In name order, as `Agents::iter` gives them. A list rather than
    /// `Agents`, which only discovery can fill, so a test builds one.
    agents: Vec<Agent>,
    home: Option<PathBuf>,
}

impl AgentsReport {
    async fn gather(cwd: &Path, paths: &Paths) -> Self {
        let context = context(cwd.to_path_buf(), paths).await;
        Self {
            agents: context.agents.iter().cloned().collect(),
            home: paths.home.clone(),
        }
    }
}

impl Report for AgentsReport {
    fn text(&self, out: &mut dyn Write) -> Result<()> {
        let width = self.agents.iter().map(|a| a.name.len()).max().unwrap_or(0);
        let groups = groups(&self.agents, |agent| (agent.source, agent.root.as_deref()));
        for ((source, root), group) in groups {
            let heading = match root {
                Some(root) => format!(
                    "{}  {}",
                    source.name().bold(),
                    place(root, self.home.as_deref()).dimmed()
                ),
                None => source.name().bold().to_string(),
            };
            writeln!(out, "{heading}")?;
            for (i, agent) in group.iter().enumerate() {
                let dir = agent.path.as_deref().and_then(Path::parent);
                let nested = match nested_folder(dir, root) {
                    Some(folder) => format!("  {}", folder.dimmed()),
                    None => String::new(),
                };
                let about = match &agent.description {
                    Some(d) => shorten(d).dimmed().to_string(),
                    None if agent.is_subagent() => "no description, so the model is not offered it"
                        .yellow()
                        .to_string(),
                    None => String::new(),
                };
                // A subagent is the usual kind; the others say what they are.
                let mode = match agent.mode {
                    Mode::Subagent => String::new(),
                    mode => format!("  {}", mode.name().magenta()),
                };
                writeln!(
                    out,
                    "{} {:width$}{mode}{nested}  {about}",
                    branch(i, group.len()).dimmed(),
                    agent.name.cyan()
                )?;
            }
        }
        Ok(())
    }

    fn json(&self, out: &mut dyn Write) -> Result<()> {
        for agent in &self.agents {
            let line = serde_json::json!({
                "name": agent.name,
                "mode": agent.mode.name(),
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nth_context::agents::Source;
    use serde_json::Value;

    use super::*;
    use crate::output::testing;

    /// An agent read from `<root>/<file>`.
    fn agent(name: &str, mode: Mode, source: Source, root: &str, file: &str) -> Agent {
        Agent {
            name: name.into(),
            mode,
            description: Some(format!("The {name} agent")),
            prompt: None,
            tools: None,
            denied: Vec::new(),
            model: None,
            clerk: false,
            hidden: false,
            path: Some(PathBuf::from(root).join(file)),
            source,
            root: Some(root.into()),
        }
    }

    fn report(agents: Vec<Agent>) -> AgentsReport {
        AgentsReport {
            agents,
            home: Some("/home/u".into()),
        }
    }

    #[test]
    fn modes_are_tagged_and_a_missing_description_warns_only_for_a_subagent() {
        // In name order, as `Agents::iter` gives them.
        let report = report(vec![
            Agent {
                description: None,
                ..agent(
                    "planner",
                    Mode::Primary,
                    Source::Project,
                    "/repo/.nth/agents",
                    "planner.md",
                )
            },
            agent(
                "research",
                Mode::All,
                Source::Global,
                "/home/u/.claude/agents",
                "research.md",
            ),
            agent(
                "review",
                Mode::Subagent,
                Source::Global,
                "/home/u/.claude/agents",
                "team/group/review.md",
            ),
            Agent {
                description: None,
                ..agent(
                    "scout",
                    Mode::Subagent,
                    Source::Project,
                    "/repo/.nth/agents",
                    "scout.md",
                )
            },
        ]);

        insta::assert_snapshot!(testing::text(&report));
    }

    #[test]
    fn a_builtin_agent_has_no_root_in_its_heading() {
        let report = report(vec![Agent {
            path: None,
            root: None,
            ..agent("general", Mode::Subagent, Source::Builtin, "/", "")
        }]);

        insta::assert_snapshot!(testing::text(&report));
    }

    #[test]
    fn each_agent_is_a_json_line() {
        let report = report(vec![
            Agent {
                tools: Some(vec!["read".into()]),
                denied: vec!["bash".into()],
                model: Some("zen/claude".into()),
                hidden: true,
                ..agent(
                    "planner",
                    Mode::Primary,
                    Source::Project,
                    "/repo/.nth/agents",
                    "planner.md",
                )
            },
            Agent {
                description: None,
                ..agent(
                    "scout",
                    Mode::Subagent,
                    Source::Global,
                    "/home/u/.claude/agents",
                    "scout.md",
                )
            },
        ]);

        let json = testing::json(&report);
        let lines: Vec<Value> = json
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["mode"], "primary");
        assert_eq!(lines[0]["source"], "project");
        assert_eq!(lines[0]["path"], "/repo/.nth/agents/planner.md");
        assert_eq!(lines[1]["mode"], "subagent");
        assert_eq!(lines[1]["description"], Value::Null);
        insta::assert_json_snapshot!(lines[0]);
    }

    #[tokio::test]
    async fn gather_finds_the_agents_under_the_working_directory() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let project = root.path().join("project");
        let folder = project.join(".nth/agents");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("scout.md"),
            "---\ndescription: Look around\nmode: primary\n---\nYou scout.\n",
        )
        .unwrap();
        let paths = Paths {
            home: Some(home.clone()),
            config_home: Some(home.join(".config")),
            data_home: Some(home.join(".local/share")),
            skill_paths: Vec::new(),
            memory: false,
        };

        let report = AgentsReport::gather(&project, &paths).await;

        assert_eq!(report.home, Some(home));
        let scout = report
            .agents
            .iter()
            .find(|a| a.name == "scout")
            .expect("the project's agent is found");
        assert_eq!(scout.source, Source::Project);
        assert_eq!(scout.mode, Mode::Primary);
        assert_eq!(
            scout.path.as_deref(),
            Some(folder.join("scout.md").as_path())
        );
    }
}
