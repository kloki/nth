use std::path::{Path, PathBuf};

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
/// The working directories added with `/add-dir`, after the environment
/// block; left out when there are none.
const DIRS: &str = include_str!("prompts/system/dirs.md");
/// The agents the model may delegate to, after the skills; a subagent has
/// no task tool, so it is not told of them.
const AGENTS: &str = include_str!("prompts/system/agents.md");
const AGENT: &str = include_str!("prompts/system/agent.md");

/// Model-id substring → its own persona, first match wins, in opencode's
/// `provider()` order; a model matching none gets the default. Drop a new file
/// in `prompts/system/` and add a row here.
const BY_MODEL: &[(&str, &str)] = &[
    ("muse", include_str!("prompts/system/meta.md")),
    // gpt-4, o1 and o3 share opencode's "beast"; gpt-6 its "astra".
    ("gpt-4", include_str!("prompts/system/beast.md")),
    ("o1", include_str!("prompts/system/beast.md")),
    ("o3", include_str!("prompts/system/beast.md")),
    ("gpt-6", include_str!("prompts/system/gpt-astra.md")),
    ("codex", include_str!("prompts/system/codex.md")),
    ("gpt", include_str!("prompts/system/gpt.md")),
    ("gemini", include_str!("prompts/system/gemini.md")),
    ("claude", include_str!("prompts/system/anthropic.md")),
    ("trinity", include_str!("prompts/system/trinity.md")),
    ("kimi", include_str!("prompts/system/kimi.md")),
    ("moonshot", include_str!("prompts/system/kimi.md")),
];

/// The persona for `model`: the first matching variant, else the default.
fn template(model: &str) -> &'static str {
    BY_MODEL
        .iter()
        .find(|(needle, _)| model.contains(needle))
        .map_or(TEMPLATE, |(_, template)| *template)
}

/// The system prompt of a session run for you: the model's persona, the
/// environment, the instruction files and the skills.
pub fn system_prompt(model: &str, cwd: &Path, context: &Context) -> String {
    render(None, model, cwd, &[], context)
}

/// The same, with the working directories added with `/add-dir`.
pub fn system_prompt_with_dirs(
    model: &str,
    cwd: &Path,
    dirs: &[PathBuf],
    context: &Context,
) -> String {
    render(None, model, cwd, dirs, context)
}

/// The system prompt of a subagent's session: `persona` in place of the
/// model's, as opencode puts an agent's prompt where the provider's would
/// go, or the model's when the agent has none; then the same environment,
/// instructions and skills, with the working directories added with
/// `/add-dir`.
pub fn subagent_system_prompt_with_dirs(
    persona: Option<&str>,
    model: &str,
    cwd: &Path,
    dirs: &[PathBuf],
    context: &Context,
) -> String {
    render(Some(persona), model, cwd, dirs, context)
}

/// `role` is `None` for your own session and `Some(persona)` for a
/// subagent's.
fn render(
    role: Option<Option<&str>>,
    model: &str,
    cwd: &Path,
    dirs: &[PathBuf],
    context: &Context,
) -> String {
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
    // The meta persona names the model itself; every other one leaves it out.
    let persona = match role {
        // Ends in a newline as the template files do, so the environment
        // block follows after a blank line either way.
        Some(Some(persona)) => format!("{}\n", persona.trim()),
        _ => template(model).replace("{{MODEL_NAME}}", model),
    };
    let mut prompt = format!("{persona}\n{env}");
    if !dirs.is_empty() {
        // One per added directory, indented as the environment's lines are.
        let list = dirs
            .iter()
            .map(|dir| format!("  - {}", dir.display()))
            .collect::<Vec<_>>()
            .join("\n");
        prompt.push('\n');
        prompt.push_str(&DIRS.replace("{dirs}", &list));
    }
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
    // An agent without a description gives the model nothing to choose by.
    let agents: Vec<String> = context
        .agents
        .iter()
        .filter_map(|agent| {
            let description = agent.description.as_deref()?;
            Some(
                AGENT
                    .replace("{name}", &escape(&agent.name))
                    .replace("{description}", &escape(description)),
            )
        })
        .collect();
    if role.is_none() && !agents.is_empty() {
        prompt.push_str(&AGENTS.replace("{agents}", agents.concat().trim_end()));
    }
    prompt
}

/// Keeps a description from closing the tags around it.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Whether `prompt`, a rendered system prompt, names today in its
/// environment block, or an earlier day.
pub(crate) fn names_today(prompt: &str) -> bool {
    prompt.contains(&format!("Today's date: {}", today()))
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
        let (skills, agents) = tail
            .split_once("</available_skills>\n")
            .expect("skills block");
        let location = dir.path().join(".agents/skills/deploy/SKILL.md");
        assert_eq!(
            skills,
            format!(
                "\nSkills provide specialized instructions and workflows for specific tasks.\n\
                 Use the skill tool to load a skill when a task matches its description.\n\
                 <available_skills>\n  <skill>\n    <name>deploy</name>\n    \
                 <description>Ship &lt;it&gt; &amp; tag</description>\n    \
                 <location>{}</location>\n  </skill>\n",
                location.display()
            )
        );
        assert!(
            agents.starts_with("Subagents handle work you delegate with the task tool."),
            "{agents}"
        );
        assert!(
            agents.contains("<agent>\n    <name>explore</name>\n    <description>Fast agent"),
            "{agents}"
        );
        assert!(agents.ends_with("</available_agents>\n"), "{agents}");
    }

    #[test]
    fn a_subagent_is_not_told_of_the_agents() {
        let context = Context::discover(
            std::path::Path::new("/nowhere"),
            &nth_context::Paths::default(),
        );

        let own = system_prompt("glm", "/repo".as_ref(), &context);
        let child = subagent_system_prompt_with_dirs(None, "glm", "/repo".as_ref(), &[], &context);

        assert!(own.contains("<available_agents>"), "{own}");
        assert!(!child.contains("<available_agents>"), "{child}");
    }

    #[test]
    fn without_instructions_the_prompt_ends_at_the_environment() {
        let prompt = system_prompt("glm", "/repo".as_ref(), &Context::default());

        assert!(prompt.ends_with("</env>\n"), "{prompt}");
    }

    #[test]
    fn added_directories_follow_the_environment() {
        let dirs = ["/elsewhere".to_string(), "/home/k/reference".to_string()];
        let dirs: Vec<_> = dirs.iter().map(PathBuf::from).collect();
        let prompt = system_prompt_with_dirs("glm", "/repo".as_ref(), &dirs, &Context::default());

        // The block is left out entirely when there is nothing added.
        assert!(
            !system_prompt("glm", "/repo".as_ref(), &Context::default())
                .contains("<additional_directories>")
        );
        let tail = prompt.split("</env>\n").nth(1).expect("env block");
        assert_eq!(
            tail,
            "\n<additional_directories>\n  You may also read and edit files in these \
             additional working directories, by their absolute paths:\n  - /elsewhere\n  \
             - /home/k/reference\n</additional_directories>\n",
            "{tail}"
        );
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
    fn a_prompt_names_today_until_the_day_changes() {
        let prompt = system_prompt("glm", "/repo".as_ref(), &Context::default());

        assert!(names_today(&prompt));
        assert!(!names_today(&prompt.replace(&today(), "Mon Jan 01 2001")));
    }

    #[test]
    fn a_model_picks_its_family_prompt_else_the_default() {
        // No match falls through to nth's own default.
        assert_eq!(template("glm-5.3"), TEMPLATE);
        assert_eq!(template("deepseek-v4.1-flash"), TEMPLATE);

        // The family starts, and opencode's order: beast before gpt/astra,
        // astra and codex before the plain gpt prompt.
        assert!(template("kimi-k3").starts_with("You are nth, an interactive general AI agent"));
        assert!(template("moonshotai/kimi-k2").starts_with("You are nth, an interactive"));
        assert!(template("gpt-4o").starts_with("You are nth, an agent"));
        assert!(template("o3-mini").starts_with("You are nth, an agent"));
        assert!(template("gpt-6").starts_with("You are an AI agent powered by nth"));
        assert!(template("gpt-5-codex").starts_with("You are nth, the best coding agent"));
        assert!(template("gpt-5.1").starts_with("You are nth."));
        assert!(template("gemini-2.5-pro").starts_with("You are nth, an interactive CLI agent"));
        assert!(template("claude-sonnet-4").starts_with("You are nth, the best coding agent"));
        assert!(template("trinity-large").starts_with("You are nth, an interactive CLI tool"));
        assert!(template("muse-glimmer").starts_with("You are nth, a coding agent"));
    }

    #[test]
    fn the_meta_prompt_is_named_for_the_model() {
        let prompt = system_prompt("muse-glimmer", "/repo".as_ref(), &Context::default());

        assert!(prompt.contains("powered by muse-glimmer,"), "{prompt}");
        assert!(!prompt.contains("{{MODEL_NAME}}"), "{prompt}");
    }

    #[test]
    fn a_subagent_gets_its_persona_or_the_models() {
        let own = subagent_system_prompt_with_dirs(
            Some("You search.\n"),
            "kimi-k3",
            "/repo".as_ref(),
            &[],
            &Context::default(),
        );
        let models = subagent_system_prompt_with_dirs(
            None,
            "kimi-k3",
            "/repo".as_ref(),
            &[],
            &Context::default(),
        );

        assert!(
            own.starts_with("You search.\n\nYou are powered by the model named kimi-k3."),
            "{own}"
        );
        assert!(!own.contains("interactive general AI agent"), "{own}");
        assert_eq!(
            models,
            system_prompt("kimi-k3", "/repo".as_ref(), &Context::default())
        );
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
