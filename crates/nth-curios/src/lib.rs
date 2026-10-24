//! Repository-local, durable static-document review. No session or UI dependencies.
mod host;
mod model;
mod store;
mod validation;

#[cfg(test)]
mod tests;

pub use host::Host;
pub use model::*;
pub use store::{Discovery, Stamp, Store, discover, repository_root};
pub use validation::{MAX_HTML_BYTES, validate};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    NotFound(String),
    #[error("Curios requires a Git repository")]
    RepoRequired,
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid",
            Self::Conflict(_) => "conflict",
            Self::NotFound(_) => "not_found",
            Self::RepoRequired => "repo_required",
            Self::Io(_) | Self::Json(_) => "unavailable",
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;
