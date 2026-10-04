//! Slash commands typed into the prompt, and picking from the ones that
//! match what has been typed so far.

use crate::popup::Popup;

/// Longest command name plus its leading `/`, so descriptions line up.
const NAME_WIDTH: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    Clear,
    Exit,
    Models,
    Resume,
}

impl Command {
    const ALL: [Command; 4] = [
        Command::Clear,
        Command::Exit,
        Command::Models,
        Command::Resume,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Command::Clear => "clear",
            Command::Exit => "exit",
            Command::Models => "models",
            Command::Resume => "resume",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Command::Clear => "start a fresh session",
            Command::Exit => "quit nth",
            Command::Models => "switch model and effort",
            Command::Resume => "reopen a past session",
        }
    }

    /// How the command shows in the completion popup.
    pub fn row(self) -> (String, &'static str) {
        (format!("/{:<NAME_WIDTH$}", self.name()), self.about())
    }

    /// Only a prompt that is exactly `/<name>` is a command. Anything else,
    /// such as `/etc/hosts what is this`, is meant for the model.
    pub fn parse(text: &str) -> Option<Command> {
        let name = text.trim().strip_prefix('/')?;
        Self::ALL.into_iter().find(|c| c.name() == name)
    }

    /// The popup for `stem`, or `None` when no command completes it.
    pub fn complete(stem: &str) -> Option<Popup<Command>> {
        Popup::new(Self::matching(stem))
    }

    /// Commands that complete `stem`, a `/` followed by part of a name.
    pub fn matching(stem: &str) -> Vec<Command> {
        match stem.strip_prefix('/') {
            Some(part) if !part.contains(char::is_whitespace) => Self::ALL
                .into_iter()
                .filter(|c| c.name().starts_with(part))
                .collect(),
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(Command::matching("/"), Command::ALL);
        assert_eq!(Command::matching("/c"), [Command::Clear]);
        assert_eq!(Command::matching("/m"), [Command::Models]);
        assert_eq!(Command::matching("/r"), [Command::Resume]);
        assert_eq!(Command::matching("/x"), []);
        assert_eq!(Command::matching("/c x"), []);
        assert_eq!(Command::matching("c"), []);
    }
}
