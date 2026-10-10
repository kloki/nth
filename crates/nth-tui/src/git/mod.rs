//! The git state of the working directory, for the status bar: the branch,
//! the branch's pull request, and a starship-style summary of what is
//! changed.

mod pr;
mod status;
mod view;

/// The state the forge reports, for tests that build a `Pr`.
#[cfg(test)]
pub(crate) use pr::State;
pub use pr::{Pr, load as load_pr};
pub use status::{GitStatus, load};
pub use view::summary;
