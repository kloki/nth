//! Title metadata shared by session owners and the store.

use serde::{Deserialize, Serialize};

mod generation;
pub use generation::{Job, Outcome};

/// A title's provenance. A user title always wins over generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Generated,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Title {
    pub text: String,
    pub origin: Origin,
}

/// Whether the first eligible prompt still has an automatic attempt available.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Generation {
    Pending,
    Attempted,
}

/// `None` on a session means a legacy save; it must not be auto-titled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metadata {
    pub revision: u64,
    pub title: Option<Title>,
    pub generation: Generation,
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            revision: 0,
            title: None,
            generation: Generation::Pending,
        }
    }
}

impl Metadata {
    /// Legacy sessions have already missed their initial generation opportunity.
    pub fn legacy() -> Self {
        Self {
            generation: Generation::Attempted,
            ..Self::default()
        }
    }

    pub fn rename(&mut self, text: &str) -> Result<(), Error> {
        let text = validate(text)?;
        self.advance()?;
        self.title = Some(Title {
            text,
            origin: Origin::User,
        });
        self.generation = Generation::Attempted;
        Ok(())
    }

    /// Apply an automatic result only against the revision it was started on.
    pub fn generated(&mut self, expected: u64, text: &str) -> Result<bool, Error> {
        if self.revision != expected
            || self
                .title
                .as_ref()
                .is_some_and(|t| t.origin == Origin::User)
        {
            return Ok(false);
        }
        let text = validate(text)?;
        self.advance()?;
        self.title = Some(Title {
            text,
            origin: Origin::Generated,
        });
        self.generation = Generation::Attempted;
        Ok(true)
    }

    /// Record an attempt before launching it, even if it later fails.
    pub fn attempted(&mut self) -> Result<(), Error> {
        if self.generation == Generation::Pending {
            self.advance()?;
            self.generation = Generation::Attempted;
        }
        Ok(())
    }

    fn advance(&mut self) -> Result<(), Error> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(Error::RevisionExhausted)?;
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("title must not be empty")]
    Empty,
    #[error("title must be a single line without control characters")]
    Control,
    #[error("title must be at most 100 characters")]
    TooLong,
    #[error("title metadata revision exhausted")]
    RevisionExhausted,
}

/// Trim a manual title and reject invalid input without silently truncating it.
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
    fn validates_manual_titles_and_rejects_stale_generation() {
        assert_eq!(validate("  résumé  ").unwrap(), "résumé");
        assert_eq!(validate(" "), Err(Error::Empty));
        assert_eq!(validate("a\nb"), Err(Error::Control));
        assert_eq!(validate(&"é".repeat(101)), Err(Error::TooLong));
        let mut metadata = Metadata::default();
        metadata.attempted().unwrap();
        let revision = metadata.revision;
        metadata.rename("mine").unwrap();
        assert!(!metadata.generated(revision, "late").unwrap());
        assert_eq!(metadata.title.unwrap().text, "mine");
    }
}
