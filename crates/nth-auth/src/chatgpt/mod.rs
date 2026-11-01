//! A ChatGPT Plus or Pro subscription, signed in to with OpenAI's Sign in
//! with ChatGPT: OAuth with PKCE in the browser (`login`), tokens in the
//! `Store` under `chatgpt`, refreshed before they run out (`refresh`).

mod host;
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

/// The first sign-in registers nth's own client; OpenAI returns the issued
/// client ID on the callback to use from then on.
const REGISTRATION_CLIENT_ID: &str = "dynamic_agent_client";
/// What OpenAI shows the user when it registers nth's client.
const AGENT_NAME: &str = "nth";
const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE: &str = "/api/accounts/authorize";
const TOKEN: &str = "/api/accounts/oauth/token";
/// The API the access token is for.
const RESOURCE: &str = "https://api.openai.com/v1";
/// What the login asks for: identity, a refresh token, and the user's
/// ChatGPT plan.
const SCOPES: &str =
    "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
/// The scope without which the access token cannot run inference.
const SHARING_SCOPE: &str = "chatgpt.tokens.use.direct";
/// The port the client's redirect URI is registered with; another is refused.
const PORT: u16 = 1455;
const USER_AGENT: &str = concat!("nth/", env!("CARGO_PKG_VERSION"));
/// Token requests are small; one this slow is not coming back.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// What a request to the Responses API is sent with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access: String,
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

    /// Whether `auth.json` holds a current login. Reads the file each time,
    /// so a login in another nth counts at once.
    pub fn connected(&self) -> bool {
        matches!(
            self.inner.store.oauth_blocking(Self::ID),
            Ok(Some(auth))
                if auth.client_id.as_deref().is_some_and(|client_id| !client_id.trim().is_empty())
                    && auth.has_session()
        )
    }

    /// Ends the renewable session, then forgets its tokens. Revocation is
    /// best effort: a network that cannot reach OpenAI must not keep you
    /// signed in locally. The issued client ID and account mapping stay,
    /// as OpenAI binds a client to its account; signing in again reuses
    /// them instead of registering another client.
    pub async fn logout(&self) -> Result<(), Error> {
        let Some(auth) = self.inner.store.oauth(Self::ID).await? else {
            return Ok(());
        };
        if let Some(client_id) = &auth.client_id {
            let _ = self.revoke(client_id, &auth.refresh).await;
        }
        let signed_out = OAuth {
            access: String::new(),
            refresh: String::new(),
            expires: 0,
            account_id: auth.account_id,
            client_id: auth.client_id,
            id_token: None,
            scopes: Vec::new(),
        };
        self.inner.store.save(Self::ID, &signed_out).await
    }

    /// POSTs `form` to the issuer's token endpoint.
    async fn token(&self, form: &[(&str, &str)]) -> Result<Tokens, Error> {
        let response = self
            .inner
            .http
            .post(format!("{}{TOKEN}", self.inner.issuer))
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

    /// The issuer's published keys, for checking the ID token.
    async fn jwks(&self) -> Result<jwt::Jwks, Error> {
        let response = self
            .inner
            .http
            .get(format!("{}/.well-known/jwks.json", self.inner.issuer))
            .send()
            .await?
            .error_for_status()?;
        Ok(response.json().await?)
    }

    /// Revokes `refresh` at the endpoint the issuer's OpenID configuration
    /// names. An empty 200 means success, including for a token that was
    /// already gone.
    async fn revoke(&self, client_id: &str, refresh: &str) -> Result<(), Error> {
        #[derive(Deserialize)]
        struct Configuration {
            revocation_endpoint: String,
        }
        let configuration: Configuration = self
            .inner
            .http
            .get(format!(
                "{}/.well-known/openid-configuration",
                self.inner.issuer
            ))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let response = self
            .inner
            .http
            .post(configuration.revocation_endpoint)
            .form(&[
                ("token", refresh),
                ("token_type_hint", "refresh_token"),
                ("client_id", client_id),
            ])
            .send()
            .await?;
        match response.status() {
            status if status.is_success() => Ok(()),
            status => Err(Error::Status {
                status: status.as_u16(),
                body: response.text().await.unwrap_or_default(),
            }),
        }
    }
}

/// The token endpoint's answer, to a login and to a refresh alike.
#[derive(Debug, Deserialize)]
struct Tokens {
    id_token: Option<String>,
    access_token: String,
    refresh_token: String,
    expires_in: Option<u64>,
    scope: Option<String>,
}

impl Tokens {
    /// As saved. The ID token, scopes and account an answer leaves out are
    /// kept from `previous`; `client_id` is the one this token set belongs
    /// to.
    fn into_oauth(self, previous: Option<&OAuth>, client_id: String) -> OAuth {
        let id_token = self
            .id_token
            .or_else(|| previous.and_then(|previous| previous.id_token.clone()));
        let mut scopes: Vec<String> = self
            .scope
            .as_deref()
            .map(|scope| scope.split_whitespace().map(String::from).collect())
            .or_else(|| previous.map(|previous| previous.scopes.clone()))
            .unwrap_or_default();
        scopes.sort();
        scopes.dedup();
        OAuth {
            account_id: jwt::account_id(id_token.as_deref(), &self.access_token)
                .or_else(|| previous.and_then(|previous| previous.account_id.clone())),
            expires: now_ms() + self.expires_in.unwrap_or(3600) * 1000,
            access: self.access_token,
            refresh: self.refresh_token,
            client_id: Some(client_id),
            id_token,
            scopes,
        }
    }
}

fn grants_sharing(scopes: &[String]) -> bool {
    scopes.iter().any(|scope| scope == SHARING_SCOPE)
}
