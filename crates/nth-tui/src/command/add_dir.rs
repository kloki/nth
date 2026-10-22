//! `/add-dir <path>`: the argument as typed, expanded to the directory it
//! names, and completed to the directories under the one being typed.

use std::path::Path;

use crate::{path, popup::Popup};

/// The argument of `/add-dir` being typed: what follows `/add-dir ` at the
/// very start of `text`. `None` for any other prompt, and for one whose
/// argument has a space in it, which no directory path has.
pub fn argument(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("/add-dir ")?;
    if rest.contains(char::is_whitespace) {
        return None;
    }
    Some(rest)
}

/// The popup of directories that complete `arg`.
pub fn complete(cwd: &Path, home: Option<&str>, arg: &str) -> Option<Popup<String>> {
    Popup::new(path::candidates(cwd, home, arg, path::Entries::Dirs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_argument_only_of_add_dir() {
        assert_eq!(argument("/add-dir "), Some(""));
        assert_eq!(argument("/add-dir /tm"), Some("/tm"));
        assert_eq!(argument("/add-dir ~"), Some("~"));
        assert_eq!(argument("/add-dir  spaced"), None, "two spaces");
        assert_eq!(argument("/add-dir /tm more"), None);
        assert_eq!(argument("/add-dir"), None, "no argument yet");
        assert_eq!(argument("/exit "), None);
        assert_eq!(argument("add-dir /tm"), None);
    }
}
