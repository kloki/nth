use std::path::Path;

use nth_context::{Context, project_root};

const TEMPLATE: &str = include_str!("prompts/system.md");
/// One per instruction file, after the environment block.
const INSTRUCTION: &str = include_str!("prompts/instruction.md");
/// The skills the model may load, after the instructions, as in opencode.
const SKILLS: &str = include_str!("prompts/skills.md");
const SKILL: &str = include_str!("prompts/skill.md");

pub fn system_prompt(model: &str, cwd: &Path, context: &Context) -> String {
    let git = if project_root(cwd).is_some() {
        "yes"
    } else {
        "no"
    };
    let mut prompt = TEMPLATE
        .replace("{model}", model)
        .replace("{cwd}", &cwd.display().to_string())
        .replace("{git}", git)
        .replace("{platform}", std::env::consts::OS);
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
}
