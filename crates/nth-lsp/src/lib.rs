//! Diagnostics from language servers, the opencode way: start the right
//! server for a file's project on first touch, tell it about every change,
//! and hand back the errors it reports so the model can fix them.
//!
//! Hand-written on purpose (no `lsp-types`): the surface nth needs is a
//! dozen structs and the base protocol's framing.

pub mod client;
pub mod config;
pub mod language;
mod pool;
pub mod report;
pub mod server;
pub mod transport;
pub mod types;
pub mod uri;

pub use client::{Client, Error};
pub use config::{LspConfig, ServerConfig};
pub use pool::{Lsp, ServerInfo, ServerState, ServerStatus};
pub use types::Diagnostic;
