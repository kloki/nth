use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderValue, Request, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use base64::Engine;
use futures::stream;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::*;

// `from_static` in a const checks each value when compiling.
const FRAME_CSP: HeaderValue = HeaderValue::from_static(
    "default-src 'none'; script-src 'none'; style-src 'unsafe-inline'; img-src data:; font-src 'none'; connect-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; sandbox allow-same-origin",
);
const SHELL_CSP: HeaderValue = HeaderValue::from_static(
    "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-src 'self'; img-src 'self' data:; base-uri 'none'; object-src 'none'; form-action 'none'; frame-ancestors 'none'",
);
const NO_STORE: HeaderValue = HeaderValue::from_static("no-store");
const NOSNIFF: HeaderValue = HeaderValue::from_static("nosniff");
const NO_REFERRER: HeaderValue = HeaderValue::from_static("no-referrer");

#[derive(Clone)]
struct Service {
    store: Store,
    cid: String,
    token: String,
    cookie_name: String,
    cookie_secret: String,
    authority: String,
}

/// A single-curio host. Dropping it cancels serving and invalidates the capability.
/// Never put `url()` in model-visible results or logs.
pub struct Host {
    address: SocketAddr,
    token: String,
    stop: CancellationToken,
    task: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
}

impl Host {
    pub async fn start(store: Store, curio_id: &str) -> Result<Self> {
        let (checked, id) = (store.clone(), curio_id.to_owned());
        offload(move || checked.snapshot(&id, None)).await?;
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
        let address = listener.local_addr()?;
        let mut random = [0; 32];
        getrandom::fill(&mut random)
            .map_err(|e| Error::Invalid(format!("Capability generation failed: {e}")))?;
        let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
        getrandom::fill(&mut random)
            .map_err(|e| Error::Invalid(format!("Session generation failed: {e}")))?;
        let cookie_secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
        let service = Arc::new(Service {
            store,
            cid: curio_id.into(),
            token: token.clone(),
            cookie_name: format!("curios_{}", uuid::Uuid::new_v4().simple()),
            cookie_secret,
            authority: address.to_string(),
        });
        let app = Router::new()
            .route("/", get(shell))
            .route("/viewer.css", get(styles))
            .route("/viewer.js", get(script))
            .route("/content/{revision}", get(content))
            .route("/api/state", get(snapshot))
            .route("/api/handoff", post(handoff))
            .route("/api/comments", post(comment))
            .route("/api/threads/{id}/messages", post(reply))
            .route("/api/threads/{id}/status", post(status))
            .route("/api/send", post(send))
            .route("/api/events", get(events))
            .layer(DefaultBodyLimit::max(64 * 1024))
            .layer(middleware::from_fn_with_state(service.clone(), protect))
            .with_state(service);
        let stop = CancellationToken::new();
        let cancel = stop.clone();
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(cancel.cancelled_owned())
                .await
        });
        Ok(Self {
            address,
            token,
            stop,
            task: Some(task),
        })
    }
    pub fn address(&self) -> SocketAddr {
        self.address
    }
    pub fn url(&self) -> String {
        format!("http://{}/#{}", self.address, self.token)
    }
    /// User-only opening URL for a revision validated by the caller.
    /// Like `url()`, this contains a capability and must never be logged.
    pub fn url_for_revision(&self, revision: u64) -> String {
        format!(
            "http://{}/?revision={revision}#{}",
            self.address, self.token
        )
    }
    pub async fn stop(mut self) -> Result<()> {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            // SSE streams may otherwise keep graceful shutdown alive indefinitely.
            task.abort();
            let _ = task.await;
        }
        Ok(())
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = match self {
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::NotFound(_) => StatusCode::NOT_FOUND,
            Error::Invalid(_) | Error::RepoRequired => StatusCode::BAD_REQUEST,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(serde_json::json!({"code": self.code(), "message": self.to_string()})),
        )
            .into_response()
    }
}
fn denied() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({"code":"permission", "message":"Unauthorized local request"})),
    )
        .into_response()
}

async fn protect(
    State(service): State<Arc<Service>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let headers = request.headers();
    if headers.get(header::HOST).and_then(|v| v.to_str().ok()) != Some(&service.authority) {
        return denied();
    }
    if let Some(origin) = headers.get(header::ORIGIN)
        && origin.to_str().ok() != Some(format!("http://{}", service.authority).as_str())
    {
        return denied();
    }
    if headers
        .get("sec-fetch-site")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == "cross-site")
    {
        return denied();
    }
    if request.uri().path().starts_with("/api/") {
        let header_auth =
            headers.get("x-curios-token").and_then(|v| v.to_str().ok()) == Some(&service.token);
        let cookie_auth = headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .any(|pair| {
                pair.trim().split_once('=').is_some_and(|(name, value)| {
                    name == service.cookie_name && value == service.cookie_secret
                })
            });
        if !header_auth && (!cookie_auth || request.uri().path() == "/api/handoff") {
            return denied();
        }
        // Ambient cookies may authenticate shell fetches, never frame navigation,
        // image loads or form submissions. Generated scripts are disabled by CSP.
        if !header_auth {
            // Non-simple header distinguishes trusted viewer fetches (including
            // state/SSE reads) from ambient-cookie navigations and subresources.
            if headers
                .get("x-curios-request")
                .and_then(|v| v.to_str().ok())
                != Some("viewer")
            {
                return denied();
            }
            if headers
                .get("sec-fetch-dest")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v != "empty")
                || headers
                    .get("sec-fetch-mode")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| !matches!(v, "cors" | "same-origin"))
                || headers
                    .get("sec-fetch-site")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v != "same-origin")
            {
                return denied();
            }
            if request.method() != axum::http::Method::GET
                && (headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
                    != Some(format!("http://{}", service.authority).as_str())
                    || !headers
                        .get(header::CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| {
                            v.split(';')
                                .next()
                                .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
                        }))
            {
                return denied();
            }
        }
    }
    let frame = request.uri().path().starts_with("/content/");
    let mut response = next.run(request).await;
    let h = response.headers_mut();
    h.insert(header::CACHE_CONTROL, NO_STORE);
    h.insert(header::X_CONTENT_TYPE_OPTIONS, NOSNIFF);
    h.insert(header::REFERRER_POLICY, NO_REFERRER);
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        if frame { FRAME_CSP } else { SHELL_CSP },
    );
    response
}
async fn shell() -> Html<&'static str> {
    Html(include_str!("../assets/viewer/index.html"))
}
async fn handoff(State(s): State<Arc<Service>>) -> impl IntoResponse {
    // Session cookie: no Max-Age/Expires; only this running host accepts it.
    (
        StatusCode::NO_CONTENT,
        [(
            header::SET_COOKIE,
            format!(
                "{}={}; Path=/api/; HttpOnly; SameSite=Strict",
                s.cookie_name, s.cookie_secret
            ),
        )],
    )
}
async fn styles() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../assets/viewer/viewer.css"),
    )
}
async fn script() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        include_str!("../assets/viewer/viewer.js"),
    )
}
async fn snapshot(
    State(s): State<Arc<Service>>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<Snapshot>> {
    let revision = query
        .get("revision")
        .map(|s| {
            s.parse::<u64>()
                .map_err(|_| Error::Invalid("Invalid revision".into()))
        })
        .transpose()?;
    Ok(Json(
        on_store(s, move |s| s.store.snapshot(&s.cid, revision)).await?,
    ))
}
async fn content(State(s): State<Arc<Service>>, Path(revision): Path<u64>) -> Result<Html<String>> {
    Ok(Html(
        on_store(s, move |s| s.store.content(&s.cid, revision)).await?,
    ))
}
async fn comment(
    State(s): State<Arc<Service>>,
    Json(input): Json<Comment>,
) -> Result<Json<Thread>> {
    Ok(Json(
        on_store(s, move |s| s.store.comment(&s.cid, input)).await?,
    ))
}
async fn reply(
    State(s): State<Arc<Service>>,
    Path(id): Path<String>,
    Json(input): Json<Reply>,
) -> Result<Json<Thread>> {
    Ok(Json(
        on_store(s, move |s| s.store.reply(&s.cid, &id, input, Author::Human)).await?,
    ))
}
#[derive(Deserialize)]
struct ChangeStatus {
    expected_version: u64,
    status: Status,
}
async fn status(
    State(s): State<Arc<Service>>,
    Path(id): Path<String>,
    Json(input): Json<ChangeStatus>,
) -> Result<Json<Thread>> {
    Ok(Json(
        on_store(s, move |s| {
            s.store
                .set_status(&s.cid, &id, input.expected_version, input.status)
        })
        .await?,
    ))
}
async fn send(State(s): State<Arc<Service>>, Json(input): Json<Send>) -> Result<Json<Batch>> {
    Ok(Json(
        on_store(s, move |s| s.store.send(&s.cid, input)).await?,
    ))
}
/// Every store call waits on its lock, which another process's write or a
/// viewer in another tab may hold, so it runs off the runtime.
async fn on_store<T: std::marker::Send + 'static>(
    s: Arc<Service>,
    f: impl FnOnce(&Service) -> Result<T> + std::marker::Send + 'static,
) -> Result<T> {
    offload(move || f(&s)).await
}
async fn events(
    State(s): State<Arc<Service>>,
) -> Sse<impl futures::Stream<Item = std::result::Result<Event, std::convert::Infallible>>> {
    // The state is read only when its file changed, not every tick; the
    // stamp is one stat, the read takes the lock.
    let stream = stream::unfold((s, None, None), |(s, mut stamp, mut last)| async move {
        loop {
            let now = s.store.stamp();
            if stamp.is_none() || now != stamp {
                stamp = now;
                if let Ok(version) = on_store(s.clone(), |s| s.store.state_version()).await
                    && last != Some(version)
                {
                    last = Some(version);
                    return Some((
                        Ok(Event::default().event("change").data(version.to_string())),
                        (s, stamp, last),
                    ));
                }
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    });
    Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    /// With one runtime thread, a handler that waited on the lock in place
    /// would stop every other request, and this test's own client, until
    /// the lock was let go.
    #[tokio::test(flavor = "current_thread")]
    async fn handlers_answer_while_the_store_is_locked() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "--quiet"])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let store = Store::new(dir.path(), "session-one").unwrap();
        let cid = store
            .publish(Publish {
                request_id: "publish".into(),
                curio_id: None,
                expected_revision: None,
                title: Some("Review".into()),
                html: "<h1 id=title>Review</h1>".into(),
            })
            .unwrap()
            .curio
            .id;
        let host = Host::start(store.clone(), &cid).await.unwrap();
        let url = host.url();
        let (base, token) = url.split_once('#').unwrap();
        let (base, token) = (base.to_owned(), token.to_owned());
        let held = store.lock().unwrap();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let holder = std::thread::spawn(move || {
            // Let go by itself, so a handler stuck on the lock fails the
            // test on time rather than hang it.
            let _ = released.recv_timeout(Duration::from_secs(3));
            drop(held);
        });
        let client = reqwest::Client::new();
        let started = Instant::now();
        let state = tokio::spawn({
            let (client, base, token) = (client.clone(), base.clone(), token.clone());
            async move {
                client
                    .get(format!("{base}api/state"))
                    .header("X-Curios-Token", token)
                    .send()
                    .await
            }
        });
        // Give the snapshot time to reach the lock.
        tokio::time::sleep(Duration::from_millis(200)).await;
        let styles = client
            .get(format!("{base}viewer.css"))
            .send()
            .await
            .unwrap();
        assert_eq!(styles.status(), 200);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "a request waited on the lock"
        );
        assert!(!state.is_finished(), "the snapshot waits for the lock");
        release.send(()).unwrap();
        let state = tokio::time::timeout(Duration::from_secs(2), state)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(state.status(), 200);
        holder.join().unwrap();
    }
}
