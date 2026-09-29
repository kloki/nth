//! Slash commands typed into the prompt, and cycling through the ones that
//! match what has been typed so far.

mod view;

pub use view::draw;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Command {
    Clear,
    Exit,
}

impl Command {
    const ALL: [Command; 2] = [Command::Clear, Command::Exit];

    pub fn name(self) -> &'static str {
        match self {
            Command::Clear => "clear",
            Command::Exit => "exit",
        }
    }

    pub fn about(self) -> &'static str {
        match self {
            Command::Clear => "start a fresh session",
            Command::Exit => "quit nth",
        }
    }

    /// Only a prompt that is exactly `/<name>` is a command. Anything else,
    /// such as `/etc/hosts what is this`, is meant for the model.
    pub fn parse(text: &str) -> Option<Command> {
        let name = text.trim().strip_prefix('/')?;
        Self::ALL.into_iter().find(|c| c.name() == name)
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

/// The commands matching the prompt when completion started, and which one
/// the prompt currently holds.
#[derive(Debug)]
pub struct Completion {
    matches: Vec<Command>,
    selected: usize,
}

impl Completion {
    /// `None` when nothing matches `stem`.
    pub fn new(stem: &str) -> Option<Self> {
        let matches = Command::matching(stem);
        (!matches.is_empty()).then_some(Self {
            matches,
            selected: 0,
        })
    }

    pub fn next(&mut self) {
        self.selected = (self.selected + 1) % self.matches.len();
    }

    pub fn selected(&self) -> Command {
        self.matches[self.selected]
    }

    pub fn len(&self) -> usize {
        self.matches.len()
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
        assert_eq!(Command::matching("/x"), []);
        assert_eq!(Command::matching("/c x"), []);
        assert_eq!(Command::matching("c"), []);
    }

    #[test]
    fn completion_cycles_through_matches() {
        assert!(Completion::new("/x").is_none());

        let mut completion = Completion::new("/").expect("matches");
        assert_eq!(completion.selected(), Command::Clear);
        completion.next();
        assert_eq!(completion.selected(), Command::Exit);
        completion.next();
        assert_eq!(completion.selected(), Command::Clear);
    }
}
