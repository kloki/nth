//! Slash commands typed into the prompt, and picking from the ones that
//! match what has been typed so far. Besides nth's own commands, every
//! skill runs as `/<name> [args]`.

mod add_dir;

pub(crate) use add_dir::{argument, complete, expand};
use nth_context::Skills;

use crate::popup::Popup;

/// Longest skill description shown in the popup, so it stays narrow.
const ABOUT_CHARS: usize = 48;
/// Rows of the command popup; enough for every command and a skill or two.
const LIMIT: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    AddDir,
    Approve,
    Clear,
    Close,
    Diagnostics,
    Exit,
    Models,
    Resume,
    Settings,
}

impl Command {
    const ALL: [Command; 9] = [
        Command::AddDir,
        Command::Approve,
        Command::Clear,
        Command::Close,
        Command::Diagnostics,
        Command::Exit,
        Command::Models,
        Command::Resume,
        Command::Settings,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Command::AddDir => "add-dir",
            Command::Approve => "approve",
            Command::Clear => "clear",
            Command::Close => "close",
            Command::Diagnostics => "diagnostics",
            Command::Exit => "exit",
            Command::Models => "models",
            Command::Resume => "resume",
            Command::Settings => "settings",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Command::AddDir => "add a working directory",
            Command::Approve => "act on the plan",
            Command::Clear => "start a fresh session",
            Command::Close => "close the content tab",
            Command::Diagnostics => "show what nth found and runs",
            Command::Exit => "quit nth",
            Command::Models => "switch model and effort",
            Command::Resume => "reopen a past session",
            Command::Settings => "toggle thinking and tool output",
        }
    }

    /// `/add-dir` is the one command that takes an argument: the directory
    /// to add.
    fn takes_arg(self) -> bool {
        matches!(self, Command::AddDir)
    }

    /// A command to run and the argument it was given: the first command is
    /// `/<name>` alone, and only `/add-dir` takes anything after it, the
    /// directory to add. `/clear now` is a prompt for the model, as ever.
    pub fn invocation(text: &str) -> Option<(Command, &str)> {
        let text = text.trim();
        let name = text.strip_prefix('/')?;
        let (name, arg) = match name.split_once(char::is_whitespace) {
            Some((name, arg)) => (name, arg.trim()),
            None => (name, ""),
        };
        let command = Self::ALL.into_iter().find(|c| c.name() == name)?;
        if !arg.is_empty() && !command.takes_arg() {
            return None;
        }
        Some((command, arg))
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
            .take(LIMIT)
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
    fn an_invocation_is_a_command_and_add_dir_its_argument() {
        let add = |text| Command::invocation(text).map(|(c, a)| (c, a.to_string()));

        assert_eq!(add("/add-dir"), Some((Command::AddDir, String::new())));
        assert_eq!(add("/add-dir /tmp"), Some((Command::AddDir, "/tmp".into())));
        assert_eq!(
            add(" /add-dir  ../shared\n"),
            Some((Command::AddDir, "../shared".into())),
            "trimmed, with its `!`-less whitespace"
        );
        assert_eq!(add("/clear"), Some((Command::Clear, String::new())));
        // Only `/add-dir` takes an argument; anything else is for the model.
        assert_eq!(add("/clear now"), None);
        assert_eq!(add("/add-dirx"), None);
        assert_eq!(add("/nope"), None);
        assert_eq!(add("clear"), None);
    }

    #[test]
    fn matches_commands_by_prefix() {
        let none = Skills::default();
        assert_eq!(Entry::matching("/", &none), builtins(&Command::ALL));
        assert_eq!(
            Entry::matching("/a", &none),
            builtins(&[Command::AddDir, Command::Approve])
        );
        assert_eq!(Entry::matching("/add", &none), builtins(&[Command::AddDir]));
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

        let names = |stem| -> Vec<_> {
            Entry::matching(stem, &skills)
                .iter()
                .map(|e| e.name().to_string())
                .collect()
        };
        assert_eq!(
            names("/"),
            [
                "add-dir",
                "approve",
                "clear",
                "close",
                "diagnostics",
                "exit",
                "models",
                "resume",
                "settings",
                "deploy",
            ],
            "the popup holds the 9 commands and one skill"
        );
        assert_eq!(names("/d"), ["diagnostics", "deploy"]);
        assert_eq!(
            names("/c"),
            ["clear", "close"],
            "the clear skill is hidden by the command"
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
