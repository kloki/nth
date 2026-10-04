//! Slash commands typed into the prompt, and picking from the ones that
//! match what has been typed so far. Besides nth's own commands, every
//! skill runs as `/<name> [args]`.

use nth_context::Skills;

use crate::{mention, popup::Popup};

/// Longest skill description shown in the popup, so it stays narrow.
const ABOUT_CHARS: usize = 48;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    Approve,
    Clear,
    Close,
    Diagnostics,
    Exit,
    Models,
    Resume,
}

impl Command {
    const ALL: [Command; 7] = [
        Command::Approve,
        Command::Clear,
        Command::Close,
        Command::Diagnostics,
        Command::Exit,
        Command::Models,
        Command::Resume,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Command::Approve => "approve",
            Command::Clear => "clear",
            Command::Close => "close",
            Command::Diagnostics => "diagnostics",
            Command::Exit => "exit",
            Command::Models => "models",
            Command::Resume => "resume",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Command::Approve => "act on the plan",
            Command::Clear => "start a fresh session",
            Command::Close => "close the content tab",
            Command::Diagnostics => "show what nth found and runs",
            Command::Exit => "quit nth",
            Command::Models => "switch model and effort",
            Command::Resume => "reopen a past session",
        }
    }

    /// Only a prompt that is exactly `/<name>` is a command. Anything else,
    /// such as `/etc/hosts what is this`, is meant for the model.
    pub fn parse(text: &str) -> Option<Command> {
        let name = text.trim().strip_prefix('/')?;
        Self::ALL.into_iter().find(|c| c.name() == name)
    }
}

/// A row of the completion popup.
#[derive(Debug, Clone, PartialEq)]
pub enum Entry {
    Builtin(Command),
    Skill { name: String, about: String },
}

impl Entry {
    pub fn name(&self) -> &str {
        match self {
            Entry::Builtin(command) => command.name(),
            Entry::Skill { name, .. } => name,
        }
    }

    fn about(&self) -> &str {
        match self {
            Entry::Builtin(command) => command.about(),
            Entry::Skill { about, .. } => about,
        }
    }

    /// The popup for `stem`, or `None` when nothing completes it.
    pub fn complete(stem: &str, skills: &Skills) -> Option<Popup<Entry>> {
        Popup::new(Self::matching(stem, skills))
    }

    /// Commands, then skills, that complete `stem`: a `/` followed by part
    /// of a name. A skill named like a command is left out, since typing
    /// its name runs the command. As many as the popup shows; typing more
    /// narrows them down.
    pub fn matching(stem: &str, skills: &Skills) -> Vec<Entry> {
        let Some(part) = stem.strip_prefix('/') else {
            return Vec::new();
        };
        if part.contains(char::is_whitespace) {
            return Vec::new();
        }
        let builtins = Command::ALL.into_iter().map(Entry::Builtin);
        let skills = skills
            .iter()
            .filter(|s| Command::ALL.iter().all(|c| c.name() != s.name))
            .map(|s| Entry::Skill {
                name: s.name.clone(),
                about: about(s.description.as_deref().unwrap_or("skill")),
            });
        builtins
            .chain(skills)
            .filter(|e| e.name().starts_with(part))
            .take(mention::LIMIT)
            .collect()
    }

    /// How `entries` show in the popup, with the descriptions lined up.
    pub fn rows(entries: &[Entry]) -> Vec<(String, &str)> {
        let width = entries.iter().map(|e| e.name().len()).max().unwrap_or(0) + 1;
        entries
            .iter()
            .map(|e| (format!("/{:<width$} ", e.name()), e.about()))
            .collect()
    }
}

/// The first line of a skill's description, short enough for the popup.
fn about(description: &str) -> String {
    let line = description.lines().next().unwrap_or_default();
    match line.char_indices().nth(ABOUT_CHARS) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use nth_context::{Context, Paths};

    use super::*;

    fn builtins(names: &[Command]) -> Vec<Entry> {
        names.iter().map(|&c| Entry::Builtin(c)).collect()
    }

    /// Skills named `names` in a temporary project.
    fn skills(names: &[&str]) -> Skills {
        let dir = tempfile::tempdir().expect("tempdir");
        for name in names {
            let skill = dir.path().join(".agents/skills").join(name);
            std::fs::create_dir_all(&skill).expect("dirs");
            std::fs::write(
                skill.join("SKILL.md"),
                format!("---\ndescription: does {name}\n---\nbody\n"),
            )
            .expect("writes");
        }
        Context::discover(dir.path(), &Paths::default()).skills
    }

    #[test]
    fn parses_only_a_bare_command() {
        assert_eq!(Command::parse("/clear"), Some(Command::Clear));
        assert_eq!(Command::parse(" /exit\n"), Some(Command::Exit));
        assert_eq!(Command::parse("/clear now"), None);
        assert_eq!(Command::parse("/nope"), None);
        assert_eq!(Command::parse("clear"), None);
    }

    #[test]
    fn matches_commands_by_prefix() {
        let none = Skills::default();
        assert_eq!(Entry::matching("/", &none), builtins(&Command::ALL));
        assert_eq!(
            Entry::matching("/c", &none),
            builtins(&[Command::Clear, Command::Close])
        );
        assert_eq!(
            Entry::matching("/d", &none),
            builtins(&[Command::Diagnostics])
        );
        assert_eq!(Entry::matching("/m", &none), builtins(&[Command::Models]));
        assert_eq!(Entry::matching("/r", &none), builtins(&[Command::Resume]));
        assert_eq!(Entry::matching("/x", &none), []);
        assert_eq!(Entry::matching("/c x", &none), []);
        assert_eq!(Entry::matching("c", &none), []);
    }

    #[test]
    fn skills_follow_the_commands() {
        let skills = skills(&["review", "clear", "deploy"]);

        let names: Vec<_> = Entry::matching("/", &skills)
            .iter()
            .map(|e| e.name().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "approve",
                "clear",
                "close",
                "diagnostics",
                "exit",
                "models",
                "resume",
                "deploy",
            ],
            "the clear skill is hidden by the command, and the popup holds 8"
        );
        assert_eq!(
            Entry::matching("/re", &skills),
            [
                Entry::Builtin(Command::Resume),
                Entry::Skill {
                    name: "review".into(),
                    about: "does review".into()
                },
            ]
        );
    }

    #[test]
    fn rows_line_up_on_the_longest_name() {
        let skills = skills(&["research-opencode"]);
        let entries = Entry::matching("/re", &skills);

        assert_eq!(
            Entry::rows(&entries),
            [
                ("/resume             ".to_string(), "reopen a past session"),
                ("/research-opencode  ".to_string(), "does research-opencode"),
            ]
        );
    }
}
