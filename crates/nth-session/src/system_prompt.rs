use std::path::Path;

use jiff::Zoned;
use nth_context::{Context, project_root};

/// The default system prompt, opencode's `default.txt`.
const TEMPLATE: &str = include_str!("prompts/system/default.md");
/// The environment block, after the persona and before the instructions. Kept
/// apart from the templates so a per-model variant replaces only the persona.
const ENV: &str = include_str!("prompts/system/env.md");
/// One per instruction file, after the environment block.
const INSTRUCTION: &str = include_str!("prompts/system/instruction.md");
/// The skills the model may load, after the instructions, as in opencode.
const SKILLS: &str = include_str!("prompts/system/skills.md");
const SKILL: &str = include_str!("prompts/system/skill.md");

/// Model-id substring → its own persona, first match wins; opencode's family
/// prompts, adapted. A model matching none gets the default. Drop a new file in
/// `prompts/system/` and add a row here.
const BY_MODEL: &[(&str, &str)] = &[
    ("kimi", include_str!("prompts/system/kimi.md")),
    ("moonshot", include_str!("prompts/system/kimi.md")),
    ("gpt", include_str!("prompts/system/gpt.md")),
    ("gemini", include_str!("prompts/system/gemini.md")),
];

/// The persona for `model`: the first matching variant, else the default.
fn template(model: &str) -> &'static str {
    BY_MODEL
        .iter()
        .find(|(needle, _)| model.contains(needle))
        .map_or(TEMPLATE, |(_, template)| *template)
}

pub fn system_prompt(model: &str, cwd: &Path, context: &Context) -> String {
    let root = project_root(cwd);
    let git = if root.is_some() { "yes" } else { "no" };
    // Outside a repository the workspace root is the working directory, as in
    // opencode, where the project defaults to the directory.
    let root = root.unwrap_or_else(|| cwd.to_path_buf());
    let env = ENV
        .replace("{model}", model)
        .replace("{cwd}", &cwd.display().to_string())
        .replace("{root}", &root.display().to_string())
        .replace("{git}", git)
        .replace("{platform}", std::env::consts::OS)
        .replace("{today}", &today());
    let mut prompt = format!("{}\n{env}", template(model));
    for instruction in &context.instructions {
        prompt.push('\n');
        // Content last, so a `{path}` inside a file is left alone.
        prompt.push_str(
            &INSTRUCTION
                .replace("{path}", &instruction.path.display().to_string())
                .replace("{content}", instruction.content.trim_end()),
        );
    }
    // A skill without a description gives the model nothing to choose by.
    let skills: Vec<String> = context
        .skills
        .iter()
        .filter_map(|skill| {
            let description = skill.description.as_deref()?;
            Some(
                SKILL
                    .replace("{name}", &escape(&skill.name))
                    .replace("{location}", &escape(&skill.path.display().to_string()))
                    .replace("{description}", &escape(description)),
            )
        })
        .collect();
    if !skills.is_empty() {
        prompt.push_str(&SKILLS.replace("{skills}", skills.concat().trim_end()));
    }
    prompt
}

/// Keeps a description from closing the tags around it.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Today, local, in the shape opencode's `Date.toDateString` shows it.
fn today() -> String {
    format_date(&Zoned::now())
}

/// A single-digit day is zero-padded (`Sep 08`), where opencode shows `Sep 8`.
fn format_date(when: &Zoned) -> String {
    when.strftime("%a %b %d %Y").to_string()
}

#[cfg(test)]
mod tests {
    use nth_context::Instruction;

    use super::*;

    #[test]
    fn instructions_follow_the_environment_in_order() {
        let context = Context {
            instructions: vec![
                Instruction {
                    path: "/home/k/.config/nth/AGENTS.md".into(),
                    content: "Be brief.\n".into(),
                },
                Instruction {
                    path: "/repo/AGENTS.md".into(),
                    content: "Use {path} literally.".into(),
                },
            ],
            ..Context::default()
        };

        let prompt = system_prompt("glm", "/repo".as_ref(), &context);

        let tail = prompt.split("</env>\n").nth(1).expect("env block");
        assert_eq!(
            tail,
            "\nInstructions from: /home/k/.config/nth/AGENTS.md\nBe brief.\n\
             \nInstructions from: /repo/AGENTS.md\nUse {path} literally.\n"
        );
    }

    #[test]
    fn described_skills_are_listed_last() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, front) in [
            ("deploy", "description: Ship <it> & tag"),
            ("quiet", "name: quiet"),
        ] {
            let skill = dir.path().join(".agents/skills").join(name);
            std::fs::create_dir_all(&skill).expect("dirs");
            std::fs::write(skill.join("SKILL.md"), format!("---\n{front}\n---\nbody\n"))
                .expect("writes");
        }
        let context = Context::discover(dir.path(), &nth_context::Paths::default());

        let prompt = system_prompt("glm", dir.path(), &context);

        let tail = prompt.split("</env>\n").nth(1).expect("env block");
        let location = dir.path().join(".agents/skills/deploy/SKILL.md");
        assert_eq!(
            tail,
            format!(
                "\nSkills provide specialized instructions and workflows for specific tasks.\n\
                 Use the skill tool to load a skill when a task matches its description.\n\
                 <available_skills>\n  <skill>\n    <name>deploy</name>\n    \
                 <description>Ship &lt;it&gt; &amp; tag</description>\n    \
                 <location>{}</location>\n  </skill>\n</available_skills>\n",
                location.display()
            )
        );
    }

    #[test]
    fn without_instructions_the_prompt_ends_at_the_environment() {
        let prompt = system_prompt("glm", "/repo".as_ref(), &Context::default());

        assert!(prompt.ends_with("</env>\n"), "{prompt}");
    }

    #[test]
    fn format_date_matches_opencodes_shape() {
        let when = "2026-09-28T12:00:00[UTC]".parse::<Zoned>().expect("zoned");

        assert_eq!(format_date(&when), "Mon Sep 28 2026");
    }

    #[test]
    fn the_env_block_names_the_workspace_root_and_today() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join(".git")).expect("git dir");
        std::fs::create_dir_all(dir.path().join("sub")).expect("subdir");

        let prompt = system_prompt("glm", &dir.path().join("sub"), &Context::default());

        // The root above the working directory, not the working directory.
        let env = prompt
            .split("<env>\n")
            .nth(1)
            .expect("env block")
            .split("</env>")
            .next()
            .expect("env block");
        assert!(
            env.contains(&format!(
                "  Workspace root folder: {}",
                dir.path().display()
            )),
            "{env}"
        );
        assert!(env.contains("  Today's date: "), "{env}");
    }

    #[test]
    fn a_model_picks_its_family_prompt_else_the_default() {
        assert_eq!(template("glm-5.3"), TEMPLATE);
        assert!(template("kimi-k3").starts_with("You are nth, an interactive general AI agent"));
        assert!(template("moonshotai/kimi-k2").starts_with("You are nth, an interactive"));
        assert!(template("gpt-5").starts_with("You are nth."));
        assert!(template("gemini-2.5-pro").starts_with("You are nth, an interactive CLI agent"));
    }

    #[test]
    fn a_family_prompt_still_carries_the_environment_block() {
        let prompt = system_prompt("kimi-k3", "/repo".as_ref(), &Context::default());

        assert!(
            prompt.contains("an interactive general AI agent"),
            "{prompt}"
        );
        assert!(prompt.contains("</env>\n"), "{prompt}");
        assert!(
            prompt.contains("\n\nYou are powered by the model named kimi-k3."),
            "the env block follows the persona after a blank line: {prompt}"
        );
    }
}
