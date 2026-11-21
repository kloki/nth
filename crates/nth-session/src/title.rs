//! What names a session: a title you give it with `/rename` or
//! `nth session rename`, or one the clerk's model makes up beside the
//! first turn (`aside`). Either is `Session.name`, saved with the rest of
//! the session, so the last save wins.

use nth_protocol::{AsideKind, Message};

use crate::{SHELL_PROMPT, Session, aside::Aside, inbox::split_notices};

/// What the model is told to do with the first prompt.
pub const INSTRUCTION: &str = include_str!("title/instruction.txt");

/// Titles are cut to this many characters, so a list or a tab header
/// never has to deal with a paragraph.
const MAX_CHARS: usize = 100;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("title must not be empty")]
    Empty,
    #[error("title must be a single line without control characters")]
    Control,
    #[error("title must be at most 100 characters")]
    TooLong,
}

/// A title you typed, trimmed, or why it cannot name a session. It is
/// never cut, so you see your own text or an error.
pub fn validate(text: &str) -> Result<String, Error> {
    if text.chars().any(char::is_control) {
        return Err(Error::Control);
    }
    let text = text.trim();
    if text.is_empty() {
        return Err(Error::Empty);
    }
    if text.chars().count() > MAX_CHARS {
        return Err(Error::TooLong);
    }
    Ok(text.to_owned())
}

/// A title out of what the model answered: its first non-blank line,
/// without terminal escapes, control characters or quotes around it, cut
/// to `MAX_CHARS`; `None` when nothing is left.
pub fn normalize(text: &str) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    let mut clean = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if ('\u{40}'..='\u{7e}').contains(&c) {
                        break;
                    }
                }
            }
        } else if !c.is_control() {
            clean.push(c);
        }
    }
    let clean = clean
        .trim()
        .trim_matches(['"', '\'', '“', '”', '‘', '’'])
        .trim();
    let capped: String = clean.chars().take(MAX_CHARS).collect();
    validate(&capped).ok()
}

/// The aside that names `session` from `prompt`, when this is the prompt
/// that should: your own session with `[session] auto_title` on, no name
/// yet, and nothing asked before it but commands you ran, so `!cmd` first
/// still gets a title. Notices in front of the prompt are not what you
/// asked, so only what follows them goes to the model.
pub fn aside(session: &Session, prompt: &str) -> Option<Aside> {
    if session.subagent || !session.env.auto_title || session.name.is_some() {
        return None;
    }
    let (_, asked) = split_notices(prompt);
    if asked.trim().is_empty() || asked_before(session) {
        return None;
    }
    Some(Aside::new(
        session,
        AsideKind::Title,
        INSTRUCTION,
        asked.trim(),
    ))
}

/// Whether a prompt of yours is in the history already, other than a
/// command you ran or notices alone.
fn asked_before(session: &Session) -> bool {
    session.messages.iter().any(|message| match message {
        Message::User(text) => text != SHELL_PROMPT && !split_notices(text).1.trim().is_empty(),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use nth_protocol::{Llm, ModelSettings};

    use super::*;

    /// A session as a front-end with `[session] auto_title` on starts it.
    fn titled() -> Session {
        let mut session = Session::new("own", ".".into());
        session.env.auto_title = true;
        session
    }

    /// A scheduled wakeup, as the inbox puts it in front of a prompt.
    const WAKEUP: &str = "<wakeup type=\"scheduled\">\nwoke\n</wakeup>";

    #[test]
    fn validates_typed_titles_without_cutting_them() {
        assert_eq!(validate("  résumé  ").unwrap(), "résumé");
        assert_eq!(validate(" "), Err(Error::Empty));
        assert_eq!(validate("a\nb"), Err(Error::Control));
        assert_eq!(validate(&"é".repeat(101)), Err(Error::TooLong));
        assert_eq!(validate(&"é".repeat(100)).unwrap().chars().count(), 100);
    }

    #[test]
    fn normalizes_what_the_model_answered() {
        assert_eq!(
            normalize("\n  “Rust \u{1b}[31mタイトル”\nextra").as_deref(),
            Some("Rust タイトル")
        );
        assert_eq!(normalize(&"é".repeat(101)).unwrap().chars().count(), 100);
        assert_eq!(normalize("\n  \n"), None);
        assert_eq!(normalize("\"\""), None);
    }

    #[test]
    fn only_the_first_prompt_of_your_own_unnamed_session_is_titled() {
        let session = titled();
        let aside = aside(&session, "first").expect("titled");
        assert_eq!(aside.kind, AsideKind::Title);
        assert_eq!(aside.input, "first");
        assert!(super::aside(&session, "  ").is_none(), "nothing asked");

        let mut disabled = titled();
        disabled.env.auto_title = false;
        assert!(super::aside(&disabled, "first").is_none());

        let mut named = titled();
        named.name = Some("mine".into());
        assert!(super::aside(&named, "first").is_none());

        let child = titled().as_subagent(None);
        assert!(super::aside(&child, "first").is_none());

        let mut second = titled();
        second.messages.push(Message::User("first".into()));
        assert!(super::aside(&second, "second").is_none());
    }

    #[test]
    fn a_command_or_notices_first_still_leave_the_title_to_the_prompt() {
        let mut session = titled();
        session.messages.push(Message::User(SHELL_PROMPT.into()));
        session.messages.push(Message::User(WAKEUP.into()));
        assert!(split_notices(WAKEUP).1.is_empty(), "notices alone");
        assert!(aside(&session, "first").is_some());
    }

    #[test]
    fn notices_in_front_of_the_prompt_are_left_out() {
        let session = titled();
        assert!(aside(&session, WAKEUP).is_none());
        let aside = aside(&session, &format!("{WAKEUP}\n\nfix the build")).unwrap();
        assert_eq!(aside.input, "fix the build");
    }

    #[test]
    fn runs_on_the_clerk() {
        let mut session = titled();
        session.env.clerk = Some(ModelSettings::bare(Llm {
            model: "clerk".into(),
            ..Llm::default()
        }));
        assert_eq!(aside(&session, "first").unwrap().model.llm.model, "clerk");
    }
}
