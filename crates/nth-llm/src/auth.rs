//! How a request proves who sends it: an API key, or a ChatGPT login whose
//! token may need refreshing first. The endpoint resolves its credentials
//! once per request; the clients only put the result on the wire.

use reqwest::RequestBuilder;

/// What an endpoint signs its requests with.
#[derive(Clone)]
pub enum Credentials {
    /// An API key, sent as a bearer token (or `x-api-key` over messages).
    Key(String),
    /// A ChatGPT subscription, for the Responses API.
    ChatGpt(nth_auth::ChatGpt),
}

/// Credentials resolved for one request.
pub(crate) enum Auth {
    Key(String),
    ChatGpt(nth_auth::Credentials),
}

impl Auth {
    /// `request` signed with a bearer token. A ChatGPT login needs nothing
    /// beside it on the public API: the conversation's cache key rides in
    /// the body as `prompt_cache_key`.
    pub(crate) fn bearer(&self, request: RequestBuilder) -> RequestBuilder {
        match self {
            Self::Key(key) => request.bearer_auth(key),
            Self::ChatGpt(credentials) => request.bearer_auth(&credentials.access),
        }
    }

    /// As `bearer`, but a key goes in `x-api-key`: OpenCode Zen refuses a
    /// bearer token on messages, as Anthropic does.
    pub(crate) fn api_key(&self, request: RequestBuilder) -> RequestBuilder {
        match self {
            Self::Key(key) => request.header("x-api-key", key),
            Self::ChatGpt(_) => self.bearer(request),
        }
    }
}
