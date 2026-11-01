//! The browser login: nth listens on a loopback port, sends you to OpenAI's
//! Sign in with ChatGPT page, and trades the code the browser brings back
//! for tokens. The callback server is a few lines of HTTP by hand, since it
//! answers one request from one browser.

use std::{net::Ipv4Addr, time::Duration};

use futures::{StreamExt, stream::FuturesUnordered};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use url::Url;

use super::{
    AGENT_NAME, AUTHORIZE, Account, ChatGpt, PORT, REGISTRATION_CLIENT_ID, RESOURCE, SCOPES,
    SHARING_SCOPE, grants_sharing, host, jwt,
    pkce::{self, Pkce},
};
use crate::{Error, OAuth};

/// How long you have to finish in the browser.
const TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Browsers open connections they never send on; one is dropped after this
/// rather than kept for the whole login.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// A request line longer than this is nothing the issuer sent.
const MAX_REQUEST_LINE: u64 = 8 * 1024;
const CALLBACK_PATH: &str = "/auth/callback";

const SUCCESS_PAGE: &str = include_str!("../../templates/success.html");
const ERROR_PAGE: &str = include_str!("../../templates/error.html");

impl ChatGpt {
    /// Signs in through the browser and saves the tokens. `on_url` gets the
    /// page to open once the callback server listens; the caller shows it
    /// and opens it (`open_browser`). Gives up after 5 minutes; dropping the
    /// future cancels it and frees the port.
    pub async fn login(&self, on_url: impl FnOnce(&str)) -> Result<Account, Error> {
        let previous = self.inner.store.oauth(Self::ID).await?;
        let listener = self.listener().await?;
        let port = listener.local_addr().map_err(Error::Callback)?.port();
        let redirect = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");
        let pkce = Pkce::new()?;
        let state = pkce::state()?;
        let nonce = pkce::state()?;
        let host = host::id(&self.inner.store).await?;
        let returning = previous.as_ref().and_then(|previous| {
            previous
                .client_id
                .as_deref()
                .filter(|client_id| !client_id.trim().is_empty())
                .map(|client_id| Returning {
                    client_id,
                    id_token: previous
                        .id_token
                        .as_deref()
                        .filter(|id_token| !id_token.trim().is_empty()),
                })
        });
        on_url(&authorize_url(
            &self.inner.issuer,
            &redirect,
            &pkce.challenge,
            &state,
            &nonce,
            returning,
            &host,
        ));
        tokio::time::timeout(
            TIMEOUT,
            self.finish(
                &listener,
                &redirect,
                &pkce,
                &state,
                &nonce,
                previous.as_ref(),
            ),
        )
        .await
        .map_err(|_| Error::Timeout)?
    }

    /// The callback listener: the registered port, or any free one when it
    /// is taken so a second nth can still sign in.
    async fn listener(&self) -> Result<TcpListener, Error> {
        let port = self.inner.port;
        match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => Ok(listener),
            // Only the registered port falls back; a caller that named
            // another one wants that port, as tests do.
            Err(_) if port == PORT => TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .map_err(|source| Error::Port { port, source }),
            Err(source) => Err(Error::Port { port, source }),
        }
    }

    /// Waits for the browser, exchanges its code and tells it how that went.
    async fn finish(
        &self,
        listener: &TcpListener,
        redirect: &str,
        pkce: &Pkce,
        state: &str,
        nonce: &str,
        previous: Option<&OAuth>,
    ) -> Result<Account, Error> {
        let (mut stream, callback) = callback(listener, state).await?;
        match self
            .complete(callback, redirect, pkce, nonce, previous)
            .await
        {
            Ok(account) => {
                respond(&mut stream, "200 OK", SUCCESS_PAGE).await;
                Ok(account)
            }
            Err(error) => {
                respond(&mut stream, "200 OK", &error_page(&error.to_string())).await;
                Err(error)
            }
        }
    }

    /// Trades the callback's code for tokens, checks that they are for this
    /// sign-in and saves them.
    async fn complete(
        &self,
        callback: Callback,
        redirect: &str,
        pkce: &Pkce,
        nonce: &str,
        previous: Option<&OAuth>,
    ) -> Result<Account, Error> {
        let saved = previous
            .and_then(|previous| previous.client_id.as_deref())
            .filter(|client_id| !client_id.trim().is_empty());
        let returned = callback
            .client_id
            .as_deref()
            .filter(|client_id| !client_id.trim().is_empty());
        if let (Some(saved), Some(returned)) = (saved, returned)
            && returned != saved
        {
            return Err(Error::Denied(
                "ChatGPT returned a different client than this connection. Connect again.".into(),
            ));
        }
        let client_id = saved
            .or(returned.filter(|client_id| *client_id != REGISTRATION_CLIENT_ID))
            .ok_or_else(|| {
                Error::Denied("ChatGPT sign-in did not return a client ID. Connect again.".into())
            })?;
        let tokens = self
            .exchange(&callback.code, redirect, &pkce.verifier, client_id)
            .await?;
        let scopes: Vec<String> = tokens
            .scope
            .as_deref()
            .unwrap_or_default()
            .split_whitespace()
            .map(String::from)
            .collect();
        if !grants_sharing(&scopes) {
            return Err(Error::Denied(format!(
                "ChatGPT sign-in finished without {SHARING_SCOPE}. Sign in again and allow token sharing, or connect OpenAI with an API key."
            )));
        }
        let id_token = tokens.id_token.clone().ok_or_else(|| {
            Error::Denied("ChatGPT sign-in did not return an ID token. Connect again.".into())
        })?;
        let claims = jwt::verify(
            &id_token,
            &self.jwks().await?,
            &self.inner.issuer,
            client_id,
            nonce,
        )?;
        // A retained ID token names the account this connection is for; a
        // sign-in that names another must not replace its credentials.
        if let Some(previous) = previous
            && let Some(subject) = previous.id_token.as_deref().and_then(jwt::subject)
            && claims.subject() != Some(subject.as_str())
        {
            return Err(Error::Denied(
                "ChatGPT signed in to a different account than this connection. Sign in again."
                    .into(),
            ));
        }
        let auth = tokens.into_oauth(previous, client_id.to_string());
        self.inner.store.save(Self::ID, &auth).await?;
        Ok(Account {
            id: auth.account_id,
            email: claims.email,
        })
    }

    async fn exchange(
        &self,
        code: &str,
        redirect: &str,
        verifier: &str,
        client_id: &str,
    ) -> Result<super::Tokens, Error> {
        self.token(&[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect),
            ("resource", RESOURCE),
        ])
        .await
    }
}

/// A returning account: its issued client ID, and the ID token kept to
/// skip the account picker (`id_token_hint`).
struct Returning<'a> {
    client_id: &'a str,
    id_token: Option<&'a str>,
}

fn authorize_url(
    issuer: &str,
    redirect: &str,
    challenge: &str,
    state: &str,
    nonce: &str,
    returning: Option<Returning<'_>>,
    host: &str,
) -> String {
    let client_id = returning
        .as_ref()
        .map(|returning| returning.client_id)
        .unwrap_or(REGISTRATION_CLIENT_ID);
    let mut pairs = vec![("client_id", client_id)];
    if returning.is_none() {
        pairs.push(("agent_name_hint", AGENT_NAME));
    }
    if let Some(hint) = returning.as_ref().and_then(|returning| returning.id_token) {
        pairs.push(("id_token_hint", hint));
    }
    pairs.extend([
        ("ext_agent_host_id", host),
        ("response_type", "code"),
        ("redirect_uri", redirect),
        ("scope", SCOPES),
        ("resource", RESOURCE),
        ("state", state),
        ("nonce", nonce),
        ("code_challenge_method", "S256"),
        ("code_challenge", challenge),
    ]);
    let mut url = format!("{issuer}{AUTHORIZE}?");
    url.push_str(
        &url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(pairs)
            .finish(),
    );
    url
}

/// What one request to the callback server asked for.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Code {
        code: String,
        client_id: Option<String>,
    },
    NotFound,
    /// The login is over: the issuer refused, or the request is not the one
    /// nth is waiting for.
    Failed {
        status: &'static str,
        message: String,
    },
}

fn outcome(target: &str, state: &str) -> Outcome {
    let Ok(url) = Url::parse("http://127.0.0.1").and_then(|base| base.join(target)) else {
        return Outcome::NotFound;
    };
    if url.path() != CALLBACK_PATH {
        return Outcome::NotFound;
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    let failed = |status, message: &str| Outcome::Failed {
        status,
        message: message.to_string(),
    };
    if let Some(error) = param("error") {
        return failed("200 OK", &param("error_description").unwrap_or(error));
    }
    let Some(code) = param("code") else {
        return failed("400 Bad Request", "Missing authorization code");
    };
    if param("state").as_deref() != Some(state) {
        return failed("400 Bad Request", "Invalid state - potential CSRF attack");
    }
    Outcome::Code {
        code,
        client_id: param("client_id"),
    }
}

/// What the browser brought back, once `state` checked out.
#[derive(Debug)]
struct Callback {
    code: String,
    client_id: Option<String>,
}

/// Serves the callback server until the browser brings a code, answering
/// everything else. Connections are read side by side, so one the browser
/// opened and never used holds nothing up.
async fn callback(listener: &TcpListener, state: &str) -> Result<(TcpStream, Callback), Error> {
    let mut reading = FuturesUnordered::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(Error::Callback)?;
                reading.push(request_target(stream));
            }
            Some(read) = reading.next() => {
                let Some((mut stream, target)) = read else {
                    continue;
                };
                match outcome(&target, state) {
                    Outcome::Code { code, client_id } => return Ok((stream, Callback { code, client_id })),
                    Outcome::NotFound => respond(&mut stream, "404 Not Found", "Not found").await,
                    Outcome::Failed { status, message } => {
                        respond(&mut stream, status, &error_page(&message)).await;
                        return Err(Error::Denied(message));
                    }
                }
            }
        }
    }
}

/// The path and query of the request on `stream`, or none when it sends no
/// request line in time.
async fn request_target(stream: TcpStream) -> Option<(TcpStream, String)> {
    let mut reader = BufReader::new(stream.take(MAX_REQUEST_LINE));
    let mut line = String::new();
    tokio::time::timeout(READ_TIMEOUT, reader.read_line(&mut line))
        .await
        .ok()?
        .ok()?;
    let target = line.split_whitespace().nth(1)?.to_string();
    Some((reader.into_inner().into_inner(), target))
}

/// Answers and closes. The browser going away first changes nothing for the
/// login, so a failed write is let go.
async fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let content_type = if body.starts_with("<!doctype html>") {
        "text/html; charset=utf-8"
    } else {
        "text/plain; charset=utf-8"
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn error_page(detail: &str) -> String {
    ERROR_PAGE.replace("{{detail}}", &escape_html(detail))
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Opens `url` in your browser, where the platform has a way to. Whether it
/// did is not known, so callers show the URL as well.
pub fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let child = std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    // Waited on aside, so it is reaped without holding up the caller.
    if let Ok(mut child) = child {
        std::thread::spawn(move || child.wait());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::{
        super::{
            AGENT_NAME, REGISTRATION_CLIENT_ID, SCOPES, SHARING_SCOPE, SignedOut, TOKEN,
            testing::{Issuer, signed},
        },
        *,
    };
    use crate::Store;

    fn params(url: &str) -> HashMap<String, String> {
        Url::parse(url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    #[test]
    fn first_sign_in_registers_nth() {
        let url = authorize_url(
            "https://auth.openai.com",
            "http://127.0.0.1:1455/auth/callback",
            "challenge",
            "state",
            "nonce",
            None,
            "urn:uuid:00000000-0000-4000-8000-000000000000",
        );
        assert!(url.starts_with("https://auth.openai.com/api/accounts/authorize?"));
        let expected: HashMap<String, String> = [
            ("client_id", REGISTRATION_CLIENT_ID),
            ("agent_name_hint", AGENT_NAME),
            (
                "ext_agent_host_id",
                "urn:uuid:00000000-0000-4000-8000-000000000000",
            ),
            ("response_type", "code"),
            ("redirect_uri", "http://127.0.0.1:1455/auth/callback"),
            ("scope", SCOPES),
            ("resource", "https://api.openai.com/v1"),
            ("state", "state"),
            ("nonce", "nonce"),
            ("code_challenge_method", "S256"),
            ("code_challenge", "challenge"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(params(&url), expected);
    }

    #[test]
    fn a_saved_client_signs_in_again_without_registering() {
        let url = authorize_url(
            "https://auth.openai.com",
            "http://127.0.0.1:1455/auth/callback",
            "challenge",
            "state",
            "nonce",
            Some(Returning {
                client_id: "oaiapp_issued",
                id_token: Some("saved-id-token"),
            }),
            "urn:uuid:00000000-0000-4000-8000-000000000000",
        );
        let params = params(&url);
        assert_eq!(params["client_id"], "oaiapp_issued");
        assert!(!params.contains_key("agent_name_hint"));
        assert_eq!(params["id_token_hint"], "saved-id-token");
        assert_eq!(
            params["ext_agent_host_id"],
            "urn:uuid:00000000-0000-4000-8000-000000000000"
        );
    }

    #[test]
    fn callback_requests() {
        assert_eq!(
            outcome("/auth/callback?code=abc&state=s&client_id=oaiapp_one", "s"),
            Outcome::Code {
                code: "abc".into(),
                client_id: Some("oaiapp_one".into())
            }
        );
        assert_eq!(
            outcome("/auth/callback?code=abc&state=s", "s"),
            Outcome::Code {
                code: "abc".into(),
                client_id: None
            }
        );
        assert_eq!(outcome("/favicon.ico", "s"), Outcome::NotFound);
        assert_eq!(
            outcome("/auth/callback?error=access_denied", "s"),
            Outcome::Failed {
                status: "200 OK",
                message: "access_denied".into()
            }
        );
        assert_eq!(
            outcome("/auth/callback?state=s", "s"),
            Outcome::Failed {
                status: "400 Bad Request",
                message: "Missing authorization code".into()
            }
        );
        assert_eq!(
            outcome("/auth/callback?code=abc&state=other", "s"),
            Outcome::Failed {
                status: "400 Bad Request",
                message: "Invalid state - potential CSRF attack".into()
            }
        );
    }

    /// A login under way, with the authorize URL it sent you to.
    struct Login {
        url: HashMap<String, String>,
        task: tokio::task::JoinHandle<Result<Account, Error>>,
    }

    impl Login {
        async fn start(chatgpt: ChatGpt) -> Self {
            let (tx, rx) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                chatgpt.login(|url| tx.send(url.to_string()).unwrap()).await
            });
            Self {
                url: params(&rx.await.unwrap()),
                task,
            }
        }

        /// What the browser gets for `query` on `path`.
        async fn visit(&self, path: &str, query: &str) -> (u16, String) {
            let redirect = Url::parse(&self.url["redirect_uri"]).unwrap();
            let port = redirect.port().unwrap();
            let response = reqwest::get(format!("http://127.0.0.1:{port}{path}?{query}"))
                .await
                .unwrap();
            (response.status().as_u16(), response.text().await.unwrap())
        }

        async fn callback(&self, query: &str) -> (u16, String) {
            self.visit(CALLBACK_PATH, query).await
        }

        /// Brings the code back as the issuer does for this login.
        async fn sign_in(&self, client_id: Option<&str>) -> (u16, String) {
            let state = self.url["state"].clone();
            let mut query = format!("code=the-code&state={state}");
            if let Some(client_id) = client_id {
                query.push_str(&format!("&client_id={client_id}"));
            }
            self.callback(&query).await
        }

        fn nonce(&self) -> &str {
            &self.url["nonce"]
        }
    }

    fn chatgpt(issuer: &Issuer, store: Store) -> ChatGpt {
        ChatGpt::with_issuer(store, &issuer.url, 0).unwrap()
    }

    /// The token endpoint's answer for a login with `nonce`.
    fn answer(issuer: &Issuer, client_id: &str, nonce: &str) -> serde_json::Value {
        json!({
            "id_token": signed(
                json!({
                    "chatgpt_account_id": "acct",
                    "email": "you@example.com",
                }),
                &issuer.url,
                client_id,
                nonce,
            ),
            "access_token": "access",
            "refresh_token": "refresh",
            "expires_in": 3600,
            "scope": format!("openid profile email offline_access resource.invoke {SHARING_SCOPE}"),
        })
    }

    fn saved(client_id: Option<&str>) -> OAuth {
        OAuth {
            access: "old-access".into(),
            refresh: "old-refresh".into(),
            expires: crate::store::now_ms() + 3_600_000,
            account_id: None,
            client_id: client_id.map(String::from),
            id_token: None,
            scopes: Vec::new(),
        }
    }

    #[tokio::test]
    async fn login_saves_the_tokens() {
        let issuer = Issuer::start(200, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        let login = Login::start(chatgpt(&issuer, store.clone())).await;
        let redirect = Url::parse(&login.url["redirect_uri"]).unwrap();
        assert_eq!(redirect.host_str(), Some("127.0.0.1"));
        assert_eq!(redirect.path(), CALLBACK_PATH);

        // A connection that never sends holds nothing up.
        let port = redirect.port().unwrap();
        let _idle = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

        let (status, _) = login.visit("/favicon.ico", "").await;
        assert_eq!(status, 404);

        // The answer names the client and nonce only the authorize URL knew.
        let client_id = "oaiapp_issued";
        issuer.respond(200, answer(&issuer, client_id, login.nonce()));
        let (status, page) = login.sign_in(Some(client_id)).await;
        assert_eq!(status, 200);
        assert!(page.contains("Authorization successful"), "{page}");

        let account = login.task.await.unwrap().unwrap();
        assert_eq!(account.id.as_deref(), Some("acct"));
        assert_eq!(account.email.as_deref(), Some("you@example.com"));

        let requests = issuer.posts();
        assert_eq!(requests.len(), 1);
        let form = &requests[0].form;
        assert_eq!(requests[0].path, TOKEN);
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["code"], "the-code");
        assert_eq!(form["redirect_uri"], login.url["redirect_uri"]);
        assert_eq!(form["client_id"], client_id);
        assert_eq!(form["resource"], RESOURCE);
        assert_eq!(
            pkce::challenge(&form["code_verifier"]),
            login.url["code_challenge"]
        );

        let saved = store.oauth(ChatGpt::ID).await.unwrap().unwrap();
        assert_eq!(saved.access, "access");
        assert_eq!(saved.refresh, "refresh");
        assert_eq!(saved.client_id.as_deref(), Some(client_id));
        assert_eq!(saved.account_id.as_deref(), Some("acct"));
        assert_eq!(
            saved.scopes,
            [
                "chatgpt.tokens.use.direct",
                "email",
                "offline_access",
                "openid",
                "profile",
                "resource.invoke",
            ]
        );
        assert!(saved.id_token.is_some());
    }

    #[tokio::test]
    async fn a_sign_in_that_names_no_client_is_refused() {
        let issuer = Issuer::start(200, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        let login = Login::start(chatgpt(&issuer, store.clone())).await;

        let (status, page) = login.sign_in(None).await;
        assert_eq!(status, 200);
        assert!(page.contains("did not return a client ID"), "{page}");

        let error = login.task.await.unwrap().unwrap_err();
        assert!(
            matches!(&error, Error::Denied(message) if message.contains("client ID")),
            "{error}"
        );
        assert!(issuer.posts().is_empty());
        assert_eq!(store.oauth(ChatGpt::ID).await.unwrap(), None);
    }

    #[tokio::test]
    async fn the_registration_client_is_not_saved_as_issued() {
        let issuer = Issuer::start(200, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        let login = Login::start(chatgpt(&issuer, store.clone())).await;

        let (status, page) = login.sign_in(Some(REGISTRATION_CLIENT_ID)).await;
        assert_eq!(status, 200);
        assert!(page.contains("did not return a client ID"), "{page}");

        let error = login.task.await.unwrap().unwrap_err();
        assert!(matches!(error, Error::Denied(_)), "{error}");
        assert!(issuer.posts().is_empty());
        assert_eq!(store.oauth(ChatGpt::ID).await.unwrap(), None);
    }

    #[tokio::test]
    async fn another_client_on_reauthorization_is_refused() {
        let issuer = Issuer::start(200, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        store
            .save(ChatGpt::ID, &saved(Some("oaiapp_issued")))
            .await
            .unwrap();
        let login = Login::start(chatgpt(&issuer, store.clone())).await;
        assert_eq!(login.url["client_id"], "oaiapp_issued");
        assert!(!login.url.contains_key("agent_name_hint"));

        let (status, page) = login.sign_in(Some("oaiapp_other")).await;
        assert_eq!(status, 200);
        assert!(page.contains("different client"), "{page}");

        let error = login.task.await.unwrap().unwrap_err();
        assert!(matches!(error, Error::Denied(_)), "{error}");
        assert!(issuer.posts().is_empty());
        let kept = store.oauth(ChatGpt::ID).await.unwrap().unwrap();
        assert_eq!(kept.refresh, "old-refresh");
    }

    #[tokio::test]
    async fn reauthorization_hints_the_saved_account() {
        let issuer = Issuer::start(200, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        let mut previous = saved(Some("oaiapp_issued"));
        previous.id_token = Some(signed(
            json!({"sub": "user"}),
            &issuer.url,
            "oaiapp_issued",
            "old-nonce",
        ));
        store.save(ChatGpt::ID, &previous).await.unwrap();
        let login = Login::start(chatgpt(&issuer, store.clone())).await;

        assert_eq!(
            login.url["id_token_hint"],
            previous.id_token.as_deref().unwrap(),
            "the saved account skips the picker"
        );

        issuer.respond(200, answer(&issuer, "oaiapp_issued", login.nonce()));
        login.sign_in(None).await;
        login.task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn an_id_token_for_another_account_is_refused() {
        let issuer = Issuer::start(200, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        let mut previous = saved(Some("oaiapp_issued"));
        previous.id_token = Some(signed(
            json!({"sub": "someone-else"}),
            &issuer.url,
            "oaiapp_issued",
            "old-nonce",
        ));
        store.save(ChatGpt::ID, &previous).await.unwrap();
        let login = Login::start(chatgpt(&issuer, store.clone())).await;

        issuer.respond(200, answer(&issuer, "oaiapp_issued", login.nonce()));
        let (status, page) = login.sign_in(None).await;
        assert_eq!(status, 200);
        assert!(page.contains("different account"), "{page}");

        let error = login.task.await.unwrap().unwrap_err();
        assert!(
            matches!(&error, Error::Denied(message) if message.contains("different account")),
            "{error}"
        );
        let kept = store.oauth(ChatGpt::ID).await.unwrap().unwrap();
        assert_eq!(kept.refresh, "old-refresh");
    }

    #[tokio::test]
    async fn reauthorization_reuses_the_saved_client_when_none_returns() {
        let issuer = Issuer::start(200, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        store
            .save(ChatGpt::ID, &saved(Some("oaiapp_issued")))
            .await
            .unwrap();
        let login = Login::start(chatgpt(&issuer, store.clone())).await;

        issuer.respond(200, answer(&issuer, "oaiapp_issued", login.nonce()));
        let (status, page) = login.sign_in(None).await;
        assert_eq!(status, 200);
        assert!(page.contains("Authorization successful"), "{page}");

        login.task.await.unwrap().unwrap();
        assert_eq!(issuer.posts()[0].form["client_id"], "oaiapp_issued");
        let kept = store.oauth(ChatGpt::ID).await.unwrap().unwrap();
        assert_eq!(kept.client_id.as_deref(), Some("oaiapp_issued"));
    }

    #[tokio::test]
    async fn a_login_without_token_sharing_is_refused() {
        let issuer = Issuer::start(200, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        let login = Login::start(chatgpt(&issuer, store.clone())).await;
        let mut answer = answer(&issuer, "oaiapp_issued", login.nonce());
        answer["scope"] = json!("openid profile email offline_access");
        issuer.respond(200, answer);

        let (status, page) = login.sign_in(Some("oaiapp_issued")).await;
        assert_eq!(status, 200);
        assert!(page.contains(SHARING_SCOPE), "{page}");

        let error = login.task.await.unwrap().unwrap_err();
        assert!(matches!(error, Error::Denied(_)), "{error}");
        assert_eq!(store.oauth(ChatGpt::ID).await.unwrap(), None);
    }

    #[tokio::test]
    async fn an_id_token_for_another_sign_in_is_refused() {
        let issuer = Issuer::start(200, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        let login = Login::start(chatgpt(&issuer, store.clone())).await;
        issuer.respond(200, answer(&issuer, "oaiapp_issued", "another-nonce"));

        let (status, page) = login.sign_in(Some("oaiapp_issued")).await;
        assert_eq!(status, 200);
        assert!(page.contains("nonce"), "{page}");

        let error = login.task.await.unwrap().unwrap_err();
        assert!(matches!(error, Error::InvalidToken(_)), "{error}");
        assert_eq!(store.oauth(ChatGpt::ID).await.unwrap(), None);
    }

    #[tokio::test]
    async fn refused_authorization_ends_the_login() {
        let issuer = Issuer::start(200, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let login = Login::start(chatgpt(&issuer, Store::at(dir.path().join("auth.json")))).await;

        let (status, page) = login
            .callback("error=access_denied&error_description=<you said no>")
            .await;
        assert_eq!(status, 200);
        assert!(page.contains("&lt;you said no&gt;"), "{page}");

        let error = login.task.await.unwrap().unwrap_err();
        assert!(
            matches!(&error, Error::Denied(m) if m == "<you said no>"),
            "{error}"
        );
        assert!(issuer.requests().is_empty());
    }

    #[tokio::test]
    async fn wrong_state_ends_the_login() {
        let issuer = Issuer::start(200, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        let login = Login::start(chatgpt(&issuer, store.clone())).await;

        let (status, page) = login.callback("code=the-code&state=forged").await;
        assert_eq!(status, 400);
        assert!(page.contains("Invalid state"), "{page}");

        let error = login.task.await.unwrap().unwrap_err();
        assert!(matches!(error, Error::Denied(_)), "{error}");
        assert!(issuer.requests().is_empty());
        assert_eq!(store.oauth(ChatGpt::ID).await.unwrap(), None);
    }

    #[tokio::test]
    async fn failed_exchange_shows_in_the_browser() {
        let issuer = Issuer::start(400, json!({"error": "invalid_grant"})).await;
        let dir = tempfile::tempdir().unwrap();
        let login = Login::start(chatgpt(&issuer, Store::at(dir.path().join("auth.json")))).await;

        let (status, page) = login.sign_in(Some("oaiapp_issued")).await;
        assert_eq!(status, 200);
        assert!(page.contains("Authorization failed"), "{page}");
        assert!(page.contains("invalid_grant"), "{page}");

        let error = login.task.await.unwrap().unwrap_err();
        assert!(
            matches!(error, Error::Status { status: 400, .. }),
            "{error}"
        );
    }

    #[tokio::test]
    async fn logout_revokes_the_refresh_token_and_keeps_the_client() {
        let issuer = Issuer::start(200, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        let mut previous = saved(Some("oaiapp_issued"));
        previous.account_id = Some("acct".into());
        previous.id_token = Some("saved-id-token".into());
        previous.scopes = vec![SHARING_SCOPE.into()];
        store.save(ChatGpt::ID, &previous).await.unwrap();
        let chatgpt = chatgpt(&issuer, store.clone());

        assert!(chatgpt.connected());
        assert_eq!(chatgpt.logout().await.unwrap(), SignedOut::Revoked);
        assert!(!chatgpt.connected());

        let kept = store.oauth(ChatGpt::ID).await.unwrap().unwrap();
        assert_eq!(kept.client_id.as_deref(), Some("oaiapp_issued"));
        assert_eq!(kept.account_id.as_deref(), Some("acct"));
        assert_eq!(kept.access, "");
        assert_eq!(kept.refresh, "");
        assert_eq!(kept.id_token, None, "signing out drops the login hint");
        assert!(kept.scopes.is_empty());
        assert!(matches!(
            chatgpt.credentials().await.unwrap_err(),
            Error::NotConnected
        ));

        let posts = issuer.posts();
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].path, "/revoke");
        assert_eq!(posts[0].form["token"], "old-refresh");
        assert_eq!(posts[0].form["token_type_hint"], "refresh_token");
        assert_eq!(posts[0].form["client_id"], "oaiapp_issued");
    }

    #[tokio::test]
    async fn logout_says_when_revocation_is_unconfirmed() {
        let issuer = Issuer::start(400, json!({"error": "invalid_request"})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        store
            .save(ChatGpt::ID, &saved(Some("oaiapp_issued")))
            .await
            .unwrap();
        let chatgpt = chatgpt(&issuer, store.clone());

        assert_eq!(chatgpt.logout().await.unwrap(), SignedOut::Unconfirmed);
        assert!(!chatgpt.connected(), "the local tokens go regardless");
        assert_eq!(issuer.posts().len(), 1, "a 4xx is not tried again");
    }

    #[tokio::test]
    async fn signing_in_after_logout_reuses_the_saved_client() {
        let issuer = Issuer::start(200, json!({})).await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        store
            .save(ChatGpt::ID, &saved(Some("oaiapp_issued")))
            .await
            .unwrap();
        let chatgpt = chatgpt(&issuer, store.clone());
        chatgpt.logout().await.unwrap();

        let login = Login::start(chatgpt).await;
        assert_eq!(login.url["client_id"], "oaiapp_issued");
        assert!(!login.url.contains_key("agent_name_hint"));
        assert!(!login.url.contains_key("id_token_hint"));

        issuer.respond(200, answer(&issuer, "oaiapp_issued", login.nonce()));
        login.sign_in(None).await;
        login.task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn taken_port_says_so() {
        let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let port = taken.local_addr().unwrap().port();
        let dir = tempfile::tempdir().unwrap();
        let chatgpt = ChatGpt::with_issuer(
            Store::at(dir.path().join("auth.json")),
            "http://127.0.0.1:1",
            port,
        )
        .unwrap();
        let error = chatgpt.login(|_| panic!("no URL without a server")).await;
        assert!(matches!(error, Err(Error::Port { port: p, .. }) if p == port));
    }
}
