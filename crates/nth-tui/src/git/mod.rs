//! The git state of the working directory, for the status bar: the branch
//! and a starship-style summary of what is changed.

mod status;
mod view;

pub use status::{GitStatus, load};
pub use view::summary;
