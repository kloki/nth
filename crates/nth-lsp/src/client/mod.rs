//! One connection to one language server for one root.
//!
//! A [`Client`] is a handle to a task that owns the connection (and the
//! child process, when there is one). Requests go to the task over `mpsc`
//! and come back on a `oneshot`; what the server says about diagnostics is
//! kept in a [`Store`] the task publishes on a `watch` channel. Dropping the
//! last handle, or cancelling the token, ends the task and kills the child.

mod task;
mod wait;

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde::Serialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    process::Command as Process,
    sync::{mpsc, oneshot, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use crate::{
    server::Launch,
    types::{Diagnostic, InitializeResult, Registration},
    uri,
};

const INITIALIZE_TIMEOUT: Duration = Duration::from_millis(45_000);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(1_000);

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("the language server connection closed")]
    Closed,
    #[error("{0} timed out")]
    Timeout(&'static str),
    #[error("server error {code}: {message}")]
    Response { code: i64, message: String },
    #[error("unexpected reply: {0}")]
    Decode(#[from] serde_json::Error),
}

/// What the server has said so far. Only the client task writes it.
#[derive(Debug, Default)]
pub(crate) struct Store {
    push: HashMap<PathBuf, Vec<Diagnostic>>,
    pull: HashMap<PathBuf, Vec<Diagnostic>>,
    /// When each file's last `publishDiagnostics` arrived, and for which
    /// document version if the server said.
    published: HashMap<PathBuf, (Instant, Option<i32>)>,
    /// Dynamic `textDocument/diagnostic` registrations, by id.
    registrations: HashMap<String, Registration>,
    /// Bumped on every registration change, so waiters can notice one.
    registrations_changed: u64,
}

impl Store {
    /// Push and pull diagnostics together, without duplicates.
    fn merged(&self, path: &Path) -> Vec<Diagnostic> {
        let both = self.push.get(path).into_iter().chain(self.pull.get(path));
        dedup(both.flatten().cloned())
    }
}

pub(crate) fn dedup(items: impl IntoIterator<Item = Diagnostic>) -> Vec<Diagnostic> {
    let mut out: Vec<Diagnostic> = Vec::new();
    for item in items {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out
}

enum Command {
    Request {
        method: &'static str,
        params: Value,
        reply: oneshot::Sender<Result<Value, Error>>,
    },
    Notify {
        method: &'static str,
        params: Value,
    },
    Touch {
        path: PathBuf,
        text: String,
        reply: oneshot::Sender<Result<i32, Error>>,
    },
    /// The handshake is done; `didChange` sends a whole-document range
    /// rather than bare text when the server syncs incrementally.
    Initialized {
        incremental: bool,
    },
    /// Pulled diagnostics to remember, by file. Acknowledged, so they are
    /// in the store before the wait that pulled them returns.
    Pulled {
        by_file: BTreeMap<PathBuf, Vec<Diagnostic>>,
        stored: oneshot::Sender<()>,
    },
}

pub struct Client {
    id: String,
    root: PathBuf,
    commands: mpsc::Sender<Command>,
    store: watch::Receiver<Store>,
    /// The server answers `textDocument/diagnostic` without registering.
    pull: bool,
}

impl Client {
    /// Starts `launch` and runs the initialize handshake.
    pub async fn spawn(id: &str, launch: Launch, cancel: CancellationToken) -> Result<Self, Error> {
        let mut child = Process::new(&launch.program)
            .args(&launch.args)
            .envs(&launch.env)
            .current_dir(&launch.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(Error::Closed);
        };
        Self::start(
            id,
            &launch.root,
            stdout,
            stdin,
            Some(child),
            launch.initialization,
            cancel,
        )
        .await
    }

    /// Runs the client over any pair of streams, which is how tests talk to
    /// a scripted server. `child` is kept alive (and killed) with the task.
    pub async fn start<R, W>(
        id: &str,
        root: &Path,
        reader: R,
        writer: W,
        child: Option<tokio::process::Child>,
        initialization: Option<Value>,
        cancel: CancellationToken,
    ) -> Result<Self, Error>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let (commands, commands_rx) = mpsc::channel(32);
        let (store_tx, store) = watch::channel(Store::default());
        let task = task::Task::new(
            root.to_path_buf(),
            writer,
            child,
            initialization.clone(),
            store_tx,
        );
        tokio::spawn(task.run(reader, commands_rx, cancel));

        let mut client = Self {
            id: id.to_string(),
            root: root.to_path_buf(),
            commands,
            store,
            pull: false,
        };
        let root_uri = uri::from_path(root);
        let params = json!({
            "rootUri": root_uri,
            "processId": std::process::id(),
            "workspaceFolders": [{ "name": "workspace", "uri": root_uri }],
            "initializationOptions": initialization.clone().unwrap_or_else(|| json!({})),
            "capabilities": {
                "window": { "workDoneProgress": true },
                "workspace": {
                    "configuration": true,
                    "didChangeWatchedFiles": { "dynamicRegistration": true },
                    "diagnostics": { "refreshSupport": false },
                },
                "textDocument": {
                    "synchronization": { "didOpen": true, "didChange": true },
                    "diagnostic": { "dynamicRegistration": true, "relatedDocumentSupport": true },
                    "publishDiagnostics": { "versionSupport": false },
                },
            },
        });
        let result = tokio::time::timeout(INITIALIZE_TIMEOUT, client.request("initialize", params))
            .await
            .map_err(|_| Error::Timeout("initialize"))??;
        let result: InitializeResult = serde_json::from_value(result)?;
        client.pull = result.capabilities.diagnostic_provider.is_some();
        // The task picks the didChange shape from this; it is sent before
        // any touch can be.
        client
            .send(Command::Initialized {
                incremental: result.capabilities.incremental_sync(),
            })
            .await?;
        client.notify("initialized", json!({})).await?;
        if let Some(settings) = initialization {
            client
                .notify(
                    "workspace/didChangeConfiguration",
                    json!({ "settings": settings }),
                )
                .await?;
        }
        Ok(client)
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The task is gone: the server exited or the connection broke.
    pub fn is_closed(&self) -> bool {
        self.commands.is_closed()
    }

    async fn send(&self, command: Command) -> Result<(), Error> {
        self.commands.send(command).await.map_err(|_| Error::Closed)
    }

    pub async fn request<P: Serialize>(
        &self,
        method: &'static str,
        params: P,
    ) -> Result<Value, Error> {
        let (reply, rx) = oneshot::channel();
        let params = serde_json::to_value(params)?;
        self.send(Command::Request {
            method,
            params,
            reply,
        })
        .await?;
        rx.await.map_err(|_| Error::Closed)?
    }

    pub async fn notify<P: Serialize>(&self, method: &'static str, params: P) -> Result<(), Error> {
        let params = serde_json::to_value(params)?;
        self.send(Command::Notify { method, params }).await
    }

    /// Tells the server about the file's current text: `didOpen` the first
    /// time, `didChange` with the next version after that. Returns the
    /// version sent.
    pub async fn open_or_change(&self, path: &Path) -> Result<i32, Error> {
        let text = tokio::fs::read_to_string(path).await?;
        let (reply, rx) = oneshot::channel();
        self.send(Command::Touch {
            path: path.to_path_buf(),
            text,
            reply,
        })
        .await?;
        rx.await.map_err(|_| Error::Closed)?
    }

    /// Everything this server has reported, by file.
    pub fn diagnostics(&self) -> BTreeMap<PathBuf, Vec<Diagnostic>> {
        let store = self.store.borrow();
        store
            .push
            .keys()
            .chain(store.pull.keys())
            .map(|path| (path.clone(), store.merged(path)))
            .collect()
    }

    /// Asks the server to shut down and exit, then lets the task end.
    pub async fn shutdown(self) {
        let ask = self.request("shutdown", Value::Null);
        if tokio::time::timeout(SHUTDOWN_TIMEOUT, ask).await.is_ok() {
            let _ = self.notify("exit", Value::Null).await;
        }
    }
}

#[cfg(test)]
mod tests;
