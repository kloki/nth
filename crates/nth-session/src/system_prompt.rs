use std::path::Path;

use nth_context::{Context, project_root};

const TEMPLATE: &str = include_str!("prompts/system.md");
/// One per instruction file, after the environment block.
const INSTRUCTION: &str = include_str!("prompts/instruction.md");

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
    prompt
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
            warnings: Vec::new(),
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
    fn without_instructions_the_prompt_ends_at_the_environment() {
        let prompt = system_prompt("glm", "/repo".as_ref(), &Context::default());

        assert!(prompt.ends_with("</env>\n"), "{prompt}");
    }
}
