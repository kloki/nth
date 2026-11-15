//! `nth skills`: the skills found for the working directory, grouped by
//! where they came from.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use nth_context::{Paths, skills::Skill};
use nth_icons::icons;
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
    let report = SkillsReport::gather(&cwd, &paths).await;
    output::print(&report, json)
}

/// The skills found for a directory, and the home the listing writes as `~`.
struct SkillsReport {
    /// In name order, as `Skills::iter` gives them. A list rather than
    /// `Skills`, which only discovery can fill, so a test builds one.
    skills: Vec<Skill>,
    home: Option<PathBuf>,
}

impl SkillsReport {
    async fn gather(cwd: &Path, paths: &Paths) -> Self {
        let context = context(cwd.to_path_buf(), paths).await;
        Self {
            skills: context.skills.iter().cloned().collect(),
            home: paths.home.clone(),
        }
    }
}

impl Report for SkillsReport {
    fn text(&self, out: &mut dyn Write) -> Result<()> {
        if self.skills.is_empty() {
            writeln!(
                out,
                "{} {}",
                icons().to.cyan().bold(),
                "no skills found".dimmed()
            )?;
            return Ok(());
        }
        let width = self.skills.iter().map(|s| s.name.len()).max().unwrap_or(0);
        let groups = groups(&self.skills, |skill| (skill.source, skill.root.as_deref()));
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
            for (i, skill) in group.iter().enumerate() {
                let nested = match nested_folder(skill.dir(), root) {
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
                    branch(i, group.len()).dimmed(),
                    skill.name.cyan()
                )?;
            }
        }
        Ok(())
    }

    fn json(&self, out: &mut dyn Write) -> Result<()> {
        for skill in &self.skills {
            let line = serde_json::json!({
                "name": skill.name,
                "description": skill.description,
                "path": skill.path(),
                "root": skill.root,
                "source": skill.source.name(),
            });
            writeln!(out, "{line}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use nth_context::skills::{Location, Source};
    use serde_json::Value;

    use super::*;
    use crate::output::testing;

    /// A skill read from `<root>/<folder>/SKILL.md`.
    fn skill(name: &str, source: Source, root: &str, folder: &str, about: Option<&str>) -> Skill {
        Skill {
            name: name.into(),
            description: about.map(Into::into),
            location: Location::File(PathBuf::from(root).join(folder).join("SKILL.md")),
            source,
            root: Some(root.into()),
        }
    }

    fn report(skills: Vec<Skill>) -> SkillsReport {
        SkillsReport {
            skills,
            home: Some("/home/u".into()),
        }
    }

    #[test]
    fn no_skills_found_says_so() {
        insta::assert_snapshot!(testing::text(&report(Vec::new())));
    }

    #[test]
    fn skills_are_grouped_by_source_and_root() {
        // In name order, as `Skills::iter` gives them.
        let report = report(vec![
            skill(
                "commit",
                Source::Project,
                "/repo/.claude/skills",
                "commit",
                Some(&format!(
                    "Write the commit message. {}",
                    "And more. ".repeat(10)
                )),
            ),
            skill(
                "diagram",
                Source::Global,
                "/home/u/.claude/skills",
                "synced/1/diagram",
                Some("Draw a diagram"),
            ),
            skill(
                "notes",
                Source::Global,
                "/home/u/.config/nth/skills",
                "notes",
                Some("Take notes"),
            ),
            skill(
                "review",
                Source::Global,
                "/home/u/.claude/skills",
                "review",
                None,
            ),
        ]);

        insta::assert_snapshot!(testing::text(&report));
    }

    #[test]
    fn an_internal_skill_has_no_root_in_its_heading() {
        let report = report(vec![Skill {
            name: "config".into(),
            description: Some("How to set up config.toml".into()),
            location: Location::Internal("body"),
            source: Source::Internal,
            root: None,
        }]);

        insta::assert_snapshot!(testing::text(&report));
    }

    #[test]
    fn each_skill_is_a_json_line() {
        let report = report(vec![
            skill(
                "diagram",
                Source::Global,
                "/home/u/.claude/skills",
                "diagram",
                Some("Draw a diagram"),
            ),
            skill(
                "review",
                Source::Project,
                "/repo/.nth/skills",
                "review",
                None,
            ),
        ]);

        let json = testing::json(&report);
        let lines: Vec<Value> = json
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["name"], "diagram");
        assert_eq!(lines[0]["source"], "global");
        assert_eq!(lines[0]["path"], "/home/u/.claude/skills/diagram/SKILL.md");
        assert_eq!(lines[1]["description"], Value::Null);
        assert_eq!(lines[1]["root"], "/repo/.nth/skills");
        insta::assert_json_snapshot!(lines[0]);
    }

    #[tokio::test]
    async fn gather_finds_the_skills_under_the_working_directory() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let project = root.path().join("project");
        let folder = project.join(".nth/skills/review");
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(
            folder.join("SKILL.md"),
            "---\nname: review\ndescription: Review a change\n---\nbody\n",
        )
        .unwrap();
        let paths = Paths {
            home: Some(home.clone()),
            config_home: Some(home.join(".config")),
            data_home: Some(home.join(".local/share")),
            skill_paths: Vec::new(),
            memory: false,
        };

        let report = SkillsReport::gather(&project, &paths).await;

        assert_eq!(report.home, Some(home));
        let review = report
            .skills
            .iter()
            .find(|s| s.name == "review")
            .expect("the project's skill is found");
        assert_eq!(review.source, Source::Project);
        assert_eq!(review.description.as_deref(), Some("Review a change"));
        assert_eq!(review.path(), Some(folder.join("SKILL.md").as_path()));
    }
}
