//! `@name` in a prompt, naming an agent: as in opencode, the model is told
//! to call the task tool with that agent, so you pick the agent and the
//! model writes the task.

use std::path::Path;

use nth_context::Agents;

/// The instruction opencode adds behind a user message that mentions an
/// agent, in a system reminder so the transcript leaves it out.
const INSTRUCTION: &str = include_str!("mention.md");

/// What to append to `text` for the agents it mentions, if any: one
/// instruction per agent, in the order mentioned. A word is a mention when
/// it starts with `@`, as the prompt's file mentions do, and names an agent
/// that is not also a path under `cwd`: a file mention wins a clash.
pub fn resolve(text: &str, agents: &Agents, cwd: &Path) -> Option<String> {
    let mut named: Vec<&str> = Vec::new();
    for word in text.split_whitespace() {
        let Some(name) = word.strip_prefix('@') else {
            continue;
        };
        // `@explore,` or `@explore.` at the end of a sentence.
        let name = name.trim_end_matches(['.', ',', ';', ':', '!', '?', ')']);
        if name.is_empty() || named.contains(&name) || agents.get(name).is_none() {
            continue;
        }
        if cwd.join(name).exists() {
            continue;
        }
        named.push(name);
    }
    if named.is_empty() {
        return None;
    }
    let instructions: Vec<String> = named
        .iter()
        .map(|name| format!("\n\n{}", INSTRUCTION.replace("{name}", name).trim_end()))
        .collect();
    Some(instructions.concat())
}

#[cfg(test)]
mod tests {
    use nth_context::{Context, Paths};

    use super::*;

    fn agents() -> Agents {
        // Nothing to find anywhere, so only the built-in agents.
        Context::discover(Path::new("/nowhere"), &Paths::default()).agents
    }

    #[test]
    fn a_mentioned_agent_becomes_an_instruction_to_call_task() {
        let dir = tempfile::tempdir().expect("tempdir");

        let appended = resolve(
            "ask @explore where tabs open, then @general.",
            &agents(),
            dir.path(),
        )
        .expect("mentions");

        assert_eq!(
            appended,
            "\n\n<system-reminder>\nUse the above message and context to generate a prompt and call the task tool with subagent: explore. Invoked by user; guaranteed to exist.\n</system-reminder>\
             \n\n<system-reminder>\nUse the above message and context to generate a prompt and call the task tool with subagent: general. Invoked by user; guaranteed to exist.\n</system-reminder>"
        );
    }

    #[test]
    fn files_unknown_names_and_mail_are_not_agents() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("explore"), "").expect("writes");

        assert_eq!(
            resolve("see @explore", &agents(), dir.path()),
            None,
            "a file"
        );
        assert_eq!(
            resolve("see @nobody and me@general.com", &agents(), dir.path()),
            None
        );
        assert_eq!(resolve("plain prompt", &agents(), dir.path()), None);
        assert!(
            resolve("@general @general", &agents(), dir.path())
                .is_some_and(|s| s.matches("subagent:").count() == 1)
        );
    }
}
