//! Validation shared by the front-ends that name a session.

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("title must not be empty")]
    Empty,
    #[error("title must be a single line without control characters")]
    Control,
    #[error("title must be at most 100 characters")]
    TooLong,
}

/// Validate a manually entered title without silently shortening it.
pub fn validate(text: &str) -> Result<String, Error> {
    if text.chars().any(char::is_control) {
        return Err(Error::Control);
    }
    let text = text.trim();
    if text.is_empty() {
        return Err(Error::Empty);
    }
    if text.chars().count() > 100 {
        return Err(Error::TooLong);
    }
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_manual_titles() {
        assert_eq!(validate("  résumé  ").unwrap(), "résumé");
        assert_eq!(validate(" "), Err(Error::Empty));
        assert_eq!(validate("a\nb"), Err(Error::Control));
        assert_eq!(validate("a\tb"), Err(Error::Control));
        assert_eq!(validate(&"é".repeat(101)), Err(Error::TooLong));
        assert_eq!(validate(&"é".repeat(100)).unwrap().chars().count(), 100);
    }
}
