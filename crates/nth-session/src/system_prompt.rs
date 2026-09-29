use std::path::Path;

const TEMPLATE: &str = include_str!("prompts/system.md");

pub fn system_prompt(model: &str, cwd: &Path) -> String {
    let git = if cwd.ancestors().any(|dir| dir.join(".git").exists()) {
        "yes"
    } else {
        "no"
    };
    TEMPLATE
        .replace("{model}", model)
        .replace("{cwd}", &cwd.display().to_string())
        .replace("{git}", git)
        .replace("{platform}", std::env::consts::OS)
}
