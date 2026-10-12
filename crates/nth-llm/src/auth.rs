//! How a request proves who sends it: an API key, or a ChatGPT login whose
//! token may need refreshing first. The endpoint resolves its credentials
//! once per request; the clients only put the result on the wire.

use reqwest::RequestBuilder;

/// What an endpoint signs its requests with.
#[derive(Clone)]
pub enum Credentials {
    /// An API key, sent as a bearer token (or `x-api-key` over messages).
    Key(String),
    /// A ChatGPT subscription, for the Codex backend only.
    ChatGpt(nth_auth::ChatGpt),
}

impl Credentials {
    /// Whether a request could be signed now. Asks the ChatGPT login each
    /// time, so connecting mid-run needs nothing rebuilt.
    pub(crate) fn connected(&self) -> bool {
        match self {
            Self::Key(_) => true,
            Self::ChatGpt(chatgpt) => chatgpt.connected(),
        }
    }

    /// What the next request is signed with, refreshing a ChatGPT token
    /// that is running out.
    pub(crate) async fn resolve(&self) -> Result<Auth, nth_auth::Error> {
        Ok(match self {
            Self::Key(key) => Auth::Key(key.clone()),
            Self::ChatGpt(chatgpt) => Auth::ChatGpt(chatgpt.credentials().await?),
        })
    }
}

/// Credentials resolved for one request.
pub(crate) enum Auth {
    Key(String),
    ChatGpt(nth_auth::Credentials),
}

impl Auth {
    /// `request` signed with a bearer token, and for ChatGPT the headers the
    /// Codex backend expects beside it. `session_id` is the conversation's,
    /// when the request belongs to one.
    pub(crate) fn bearer(
        &self,
        request: RequestBuilder,
        session_id: Option<&str>,
    ) -> RequestBuilder {
        match self {
            Self::Key(key) => request.bearer_auth(key),
            Self::ChatGpt(credentials) => {
                let mut request = request
                    .bearer_auth(&credentials.access)
                    .header("originator", "nth");
                if let Some(account_id) = &credentials.account_id {
                    request = request.header("ChatGPT-Account-Id", account_id);
                }
                if let Some(session_id) = session_id {
                    request = request.header("session-id", session_id);
                }
                request
            }
        }
    }

    /// As `bearer`, but a key goes in `x-api-key`: OpenCode Zen refuses a
    /// bearer token on messages, as Anthropic does.
    pub(crate) fn api_key(
        &self,
        request: RequestBuilder,
        session_id: Option<&str>,
    ) -> RequestBuilder {
        match self {
            Self::Key(key) => request.header("x-api-key", key),
            Self::ChatGpt(_) => self.bearer(request, session_id),
        }
    }
}
