//! A ChatGPT Plus or Pro subscription, signed in to as the Codex CLI is:
//! OAuth with PKCE in the browser (`login`), tokens in the `Store` under
//! `chatgpt`, refreshed before they run out (`refresh`).

mod jwt;
mod login;
mod pkce;
mod refresh;
#[cfg(test)]
mod testing;

use std::{sync::Arc, time::Duration};

use serde::Deserialize;
use tokio::sync::Mutex;

pub use self::login::open_browser;
use crate::{Error, OAuth, Store, store::now_ms};

/// The Codex CLI's OAuth client, which OpenAI lets other harnesses sign in
/// with.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
/// The port the client's redirect URI is registered with; another is refused.
const PORT: u16 = 1455;
const USER_AGENT: &str = concat!("nth/", env!("CARGO_PKG_VERSION"));
/// Token requests are small; one this slow is not coming back.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// What a request to the Codex backend is sent with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access: String,
    /// Sent as `ChatGPT-Account-Id`, picking the workspace when you are in
    /// more than one.
    pub account_id: Option<String>,
}

/// Who a login signed in as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: Option<String>,
    pub email: Option<String>,
}

/// The ChatGPT login, cheap to clone: every clone shares one lock, so
/// requests that find the token expiring refresh it once between them.
#[derive(Debug, Clone)]
pub struct ChatGpt {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    store: Store,
    issuer: String,
    port: u16,
    http: reqwest::Client,
    refreshing: Mutex<()>,
}

impl ChatGpt {
    /// Its entry in the `Store`, and its provider id.
    pub const ID: &str = "chatgpt";

    pub fn new(store: Store) -> Result<Self, Error> {
        Self::with_issuer(store, ISSUER, PORT)
    }

    /// Signs in at `issuer` with the callback on `port` (0 for any free
    /// one): what tests point at a local server.
    pub fn with_issuer(store: Store, issuer: impl Into<String>, port: u16) -> Result<Self, Error> {
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(REQUEST_TIMEOUT)
            .build()?;
        Ok(Self {
            inner: Arc::new(Inner {
                store,
                issuer: issuer.into(),
                port,
                http,
                refreshing: Mutex::new(()),
            }),
        })
    }

    /// Whether `auth.json` holds tokens for ChatGPT. Reads the file each
    /// time, so a login in another nth counts at once.
    pub fn connected(&self) -> bool {
        matches!(self.inner.store.oauth_blocking(Self::ID), Ok(Some(_)))
    }

    pub async fn logout(&self) -> Result<(), Error> {
        self.inner.store.remove(Self::ID).await
    }

    /// POSTs `form` to the issuer's token endpoint.
    async fn token(&self, form: &[(&str, &str)]) -> Result<Tokens, Error> {
        let response = self
            .inner
            .http
            .post(format!("{}/oauth/token", self.inner.issuer))
            .form(form)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Status {
                status: status.as_u16(),
                body,
            });
        }
        Ok(response.json().await?)
    }
}

/// The token endpoint's answer, to a login and to a refresh alike.
#[derive(Debug, Deserialize)]
struct Tokens {
    id_token: Option<String>,
    access_token: String,
    refresh_token: String,
    expires_in: Option<u64>,
}

impl Tokens {
    /// As saved, with `account_id` kept when the new tokens name none.
    fn into_oauth(self, account_id: Option<String>) -> OAuth {
        OAuth {
            account_id: jwt::account_id(self.id_token.as_deref(), &self.access_token)
                .or(account_id),
            expires: now_ms() + self.expires_in.unwrap_or(3600) * 1000,
            access: self.access_token,
            refresh: self.refresh_token,
        }
    }
}
