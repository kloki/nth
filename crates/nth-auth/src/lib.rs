//! Signing in to providers that take an account rather than an API key.
//! `Store` is `auth.json`, the tokens nth keeps between runs; `ChatGpt` is
//! the browser login to a ChatGPT subscription and the handle a provider
//! asks for a fresh access token before each request.

mod chatgpt;
mod store;

use std::{io, path::PathBuf};

pub use chatgpt::{Account, ChatGpt, Credentials, open_browser};
pub use store::{OAuth, Store};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("neither XDG_DATA_HOME nor HOME is set, so nth has nowhere to keep credentials")]
    NoDataDir,
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    #[error("{}: {source}", path.display())]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("ChatGPT is not connected: run /connect or `nth connect`")]
    NotConnected,
    #[error("no randomness from the OS: {0}")]
    Random(getrandom::Error),
    #[error("port {port} is taken, so the browser has nowhere to come back to: {source}")]
    Port { port: u16, source: io::Error },
    #[error("the login callback server failed: {0}")]
    Callback(io::Error),
    #[error("authorization failed: {0}")]
    Denied(String),
    #[error("authorization took longer than 5 minutes")]
    Timeout,
    #[error("token request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("token request answered {status}: {body}")]
    Status { status: u16, body: String },
}
