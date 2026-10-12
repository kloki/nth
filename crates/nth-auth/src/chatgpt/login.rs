//! The browser login: nth listens on localhost, sends you to OpenAI's
//! authorize page, and trades the code the browser brings back for tokens.
//! The callback server is a few lines of HTTP by hand, since it answers one
//! request from one browser.

use std::{net::Ipv4Addr, time::Duration};

use futures::{StreamExt, stream::FuturesUnordered};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
};
use url::Url;

use super::{
    Account, CLIENT_ID, ChatGpt, jwt,
    pkce::{self, Pkce},
};
use crate::Error;

/// How long you have to finish in the browser.
const TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// Browsers open connections they never send on; one is dropped after this
/// rather than kept for the whole login.
const READ_TIMEOUT: Duration = Duration::from_secs(10);
/// A request line longer than this is nothing the issuer sent.
const MAX_REQUEST_LINE: u64 = 8 * 1024;
const SCOPE: &str = "openid profile email offline_access";
const CALLBACK_PATH: &str = "/auth/callback";

const SUCCESS_PAGE: &str = include_str!("../../templates/success.html");
const ERROR_PAGE: &str = include_str!("../../templates/error.html");

impl ChatGpt {
    /// Signs in through the browser and saves the tokens. `on_url` gets the
    /// page to open once the callback server listens; the caller shows it
    /// and opens it (`open_browser`). Gives up after 5 minutes; dropping the
    /// future cancels it and frees the port.
    pub async fn login(&self, on_url: impl FnOnce(&str)) -> Result<Account, Error> {
        let port = self.inner.port;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|source| Error::Port { port, source })?;
        let port = listener.local_addr().map_err(Error::Callback)?.port();
        let redirect = format!("http://localhost:{port}{CALLBACK_PATH}");
        let pkce = Pkce::new()?;
        let state = pkce::state()?;
        on_url(&authorize_url(
            &self.inner.issuer,
            &redirect,
            &pkce.challenge,
            &state,
        ));
        tokio::time::timeout(TIMEOUT, self.finish(&listener, &redirect, &pkce, &state))
            .await
            .map_err(|_| Error::Timeout)?
    }

    /// Waits for the browser, exchanges its code and tells it how that went.
    async fn finish(
        &self,
        listener: &TcpListener,
        redirect: &str,
        pkce: &Pkce,
        state: &str,
    ) -> Result<Account, Error> {
        let (mut stream, code) = callback(listener, state).await?;
        match self.exchange(&code, redirect, &pkce.verifier).await {
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

    async fn exchange(&self, code: &str, redirect: &str, verifier: &str) -> Result<Account, Error> {
        let tokens = self
            .token(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect),
                ("client_id", CLIENT_ID),
                ("code_verifier", verifier),
            ])
            .await?;
        let email = tokens.id_token.as_deref().and_then(jwt::email);
        let auth = tokens.into_oauth(None);
        self.inner.store.save(Self::ID, &auth).await?;
        Ok(Account {
            id: auth.account_id,
            email,
        })
    }
}

fn authorize_url(issuer: &str, redirect: &str, challenge: &str, state: &str) -> String {
    let mut url = format!("{issuer}/oauth/authorize");
    url.push('?');
    url.push_str(
        &url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs([
                ("response_type", "code"),
                ("client_id", CLIENT_ID),
                ("redirect_uri", redirect),
                ("scope", SCOPE),
                ("code_challenge", challenge),
                ("code_challenge_method", "S256"),
                ("id_token_add_organizations", "true"),
                ("codex_cli_simplified_flow", "true"),
                ("state", state),
                ("originator", "nth"),
            ])
            .finish(),
    );
    url
}

/// What one request to the callback server asked for.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Code(String),
    NotFound,
    /// The login is over: the issuer refused, or the request is not the one
    /// nth is waiting for.
    Failed {
        status: &'static str,
        message: String,
    },
}

fn outcome(target: &str, state: &str) -> Outcome {
    let Ok(url) = Url::parse("http://localhost").and_then(|base| base.join(target)) else {
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
    Outcome::Code(code)
}

/// Serves the callback server until the browser brings a code, answering
/// everything else. Connections are read side by side, so one the browser
/// opened and never used holds nothing up.
async fn callback(listener: &TcpListener, state: &str) -> Result<(TcpStream, String), Error> {
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
                    Outcome::Code(code) => return Ok((stream, code)),
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
        super::{jwt::token, testing::Issuer},
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
    fn authorize_url_asks_as_codex_does() {
        let url = authorize_url(
            "https://auth.openai.com",
            "http://localhost:1455/auth/callback",
            "challenge",
            "state",
        );
        assert!(url.starts_with("https://auth.openai.com/oauth/authorize?"));
        let expected: HashMap<String, String> = [
            ("response_type", "code"),
            ("client_id", "app_EMoamEEZ73f0CkXaXp7hrann"),
            ("redirect_uri", "http://localhost:1455/auth/callback"),
            ("scope", "openid profile email offline_access"),
            ("code_challenge", "challenge"),
            ("code_challenge_method", "S256"),
            ("id_token_add_organizations", "true"),
            ("codex_cli_simplified_flow", "true"),
            ("state", "state"),
            ("originator", "nth"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        assert_eq!(params(&url), expected);
    }

    #[test]
    fn callback_requests() {
        assert_eq!(
            outcome("/auth/callback?code=abc&state=s", "s"),
            Outcome::Code("abc".into())
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
    }

    fn chatgpt(issuer: &Issuer, store: Store) -> ChatGpt {
        ChatGpt::with_issuer(store, &issuer.url, 0).unwrap()
    }

    #[tokio::test]
    async fn login_saves_the_tokens() {
        let id_token = token(json!({
            "chatgpt_account_id": "acct",
            "email": "you@example.com",
        }));
        let issuer = Issuer::start(
            200,
            json!({
                "id_token": id_token,
                "access_token": "access",
                "refresh_token": "refresh",
                "expires_in": 3600,
            }),
        )
        .await;
        let dir = tempfile::tempdir().unwrap();
        let store = Store::at(dir.path().join("auth.json"));
        let login = Login::start(chatgpt(&issuer, store.clone())).await;
        let redirect = Url::parse(&login.url["redirect_uri"]).unwrap();
        assert_eq!(redirect.host_str(), Some("localhost"));
        assert_eq!(redirect.path(), CALLBACK_PATH);

        // A connection that never sends holds nothing up.
        let port = redirect.port().unwrap();
        let _idle = TcpStream::connect(("127.0.0.1", port)).await.unwrap();

        let (status, _) = login.visit("/favicon.ico", "").await;
        assert_eq!(status, 404);

        let state = login.url["state"].clone();
        let (status, page) = login
            .callback(&format!("code=the-code&state={state}"))
            .await;
        assert_eq!(status, 200);
        assert!(page.contains("Authorization successful"), "{page}");

        let account = login.task.await.unwrap().unwrap();
        assert_eq!(account.id.as_deref(), Some("acct"));
        assert_eq!(account.email.as_deref(), Some("you@example.com"));

        let requests = issuer.requests();
        assert_eq!(requests.len(), 1);
        let form = &requests[0].form;
        assert_eq!(requests[0].path, "/oauth/token");
        assert_eq!(form["grant_type"], "authorization_code");
        assert_eq!(form["code"], "the-code");
        assert_eq!(form["redirect_uri"], login.url["redirect_uri"]);
        assert_eq!(form["client_id"], CLIENT_ID);
        assert_eq!(
            pkce::challenge(&form["code_verifier"]),
            login.url["code_challenge"]
        );

        let saved = store.oauth(ChatGpt::ID).await.unwrap().unwrap();
        assert_eq!(saved.access, "access");
        assert_eq!(saved.refresh, "refresh");
        assert_eq!(saved.account_id.as_deref(), Some("acct"));
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

        let state = login.url["state"].clone();
        let (status, page) = login
            .callback(&format!("code=the-code&state={state}"))
            .await;
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
