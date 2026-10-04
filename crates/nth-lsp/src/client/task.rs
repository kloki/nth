//! The task behind a [`Client`](super::Client): the only place that writes
//! to the server, reads from it, or changes the [`Store`].

use std::{collections::HashMap, path::PathBuf};

use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncWrite, BufReader},
    process::Child,
    sync::{mpsc, oneshot, watch},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use super::{Command, Error, Store};
use crate::{
    language,
    transport::{read_message, write_message},
    types::{
        ConfigurationParams, ContentChange, DidChangeParams, DidChangeWatchedFilesParams,
        DidOpenParams, FILE_CHANGED, FILE_CREATED, FileEvent, Position, PublishDiagnosticsParams,
        Range, RegistrationParams, TextDocumentItem, UnregistrationParams,
        VersionedTextDocumentIdentifier,
    },
    uri,
};

const DIAGNOSTIC_METHOD: &str = "textDocument/diagnostic";
const METHOD_NOT_FOUND: i64 = -32601;

/// A JSON-RPC message from the server: a request when it has both `id` and
/// `method`, a notification with only `method`, a response with only `id`.
#[derive(Debug, Deserialize)]
struct Incoming {
    id: Option<Value>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
    result: Option<Value>,
    error: Option<ResponseError>,
}

#[derive(Debug, Deserialize)]
struct ResponseError {
    code: i64,
    message: String,
}

struct Document {
    version: i32,
    text: String,
}

pub(super) struct Task<W> {
    root: PathBuf,
    writer: W,
    /// Held so the server lives exactly as long as this task.
    child: Option<Child>,
    initialization: Option<Value>,
    store: watch::Sender<Store>,
    next_id: i64,
    pending: HashMap<i64, oneshot::Sender<Result<Value, Error>>>,
    documents: HashMap<PathBuf, Document>,
    incremental: bool,
}

/// The server's messages as a stream, so a read in progress survives the
/// other `select!` branches winning.
fn messages<R: AsyncRead + Unpin>(reader: R) -> impl Stream<Item = Vec<u8>> {
    futures::stream::unfold(BufReader::new(reader), |mut reader| async move {
        let body = read_message(&mut reader).await.ok()??;
        Some((body, reader))
    })
}

impl<W: AsyncWrite + Unpin> Task<W> {
    pub fn new(
        root: PathBuf,
        writer: W,
        child: Option<Child>,
        initialization: Option<Value>,
        store: watch::Sender<Store>,
    ) -> Self {
        Self {
            root,
            writer,
            child,
            initialization,
            store,
            next_id: 0,
            pending: HashMap::new(),
            documents: HashMap::new(),
            incremental: false,
        }
    }

    pub async fn run<R: AsyncRead + Unpin>(
        mut self,
        reader: R,
        mut commands: mpsc::Receiver<Command>,
        cancel: CancellationToken,
    ) {
        let mut incoming = std::pin::pin!(messages(reader));
        loop {
            let step = tokio::select! {
                _ = cancel.cancelled() => break,
                command = commands.recv() => match command {
                    Some(command) => self.command(command).await,
                    None => break,
                },
                body = incoming.next() => match body {
                    Some(body) => self.incoming(&body).await,
                    None => break,
                },
            };
            if step.is_err() {
                break;
            }
        }
        // Dropping `pending` tells every waiting request the connection is
        // gone; dropping `child` kills the server.
        drop(self.child.take());
    }

    async fn send(&mut self, message: Value) -> Result<(), Error> {
        write_message(&mut self.writer, &message).await?;
        Ok(())
    }

    async fn notify(&mut self, method: &str, params: impl serde::Serialize) -> Result<(), Error> {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
    }

    async fn command(&mut self, command: Command) -> Result<(), Error> {
        match command {
            Command::Request {
                method,
                params,
                reply,
            } => {
                // Requests whose caller gave up never get cleaned otherwise.
                self.pending.retain(|_, waiting| !waiting.is_closed());
                self.next_id += 1;
                let id = self.next_id;
                self.pending.insert(id, reply);
                self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
                    .await
            }
            Command::Notify { method, params } => self.notify(method, params).await,
            Command::Initialized { incremental } => {
                self.incremental = incremental;
                Ok(())
            }
            Command::Touch { path, text, reply } => {
                let version = self.touch(path, text).await?;
                let _ = reply.send(Ok(version));
                Ok(())
            }
            Command::Pulled { by_file, stored } => {
                self.store.send_modify(|store| store.pull.extend(by_file));
                let _ = stored.send(());
                Ok(())
            }
        }
    }

    async fn touch(&mut self, path: PathBuf, text: String) -> Result<i32, Error> {
        let uri = uri::from_path(&path);
        let Some(document) = self.documents.get(&path) else {
            self.watched(&uri, FILE_CREATED).await?;
            self.store.send_modify(|store| {
                store.push.remove(&path);
                store.pull.remove(&path);
            });
            let params = DidOpenParams {
                text_document: TextDocumentItem {
                    uri: &uri,
                    language_id: language::id(&path),
                    version: 0,
                    text: &text,
                },
            };
            self.notify("textDocument/didOpen", params).await?;
            self.documents.insert(path, Document { version: 0, text });
            return Ok(0);
        };
        // Diagnostics stay: some servers (clangd) only publish again when
        // the text really changed, and a no-op touch must not lose them.
        let version = document.version + 1;
        let range = self.incremental.then(|| Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: end_of(&document.text),
        });
        self.watched(&uri, FILE_CHANGED).await?;
        let params = DidChangeParams {
            text_document: VersionedTextDocumentIdentifier { uri: &uri, version },
            content_changes: vec![ContentChange { range, text: &text }],
        };
        self.notify("textDocument/didChange", params).await?;
        self.documents.insert(path, Document { version, text });
        Ok(version)
    }

    async fn watched(&mut self, uri: &str, kind: u8) -> Result<(), Error> {
        let params = DidChangeWatchedFilesParams {
            changes: vec![FileEvent { uri, kind }],
        };
        self.notify("workspace/didChangeWatchedFiles", params).await
    }

    async fn incoming(&mut self, body: &[u8]) -> Result<(), Error> {
        // A message nth cannot read is skipped; the framing is still intact.
        let Ok(message) = serde_json::from_slice::<Incoming>(body) else {
            return Ok(());
        };
        match (message.id, message.method) {
            (Some(id), Some(method)) => {
                let reply = match self.server_request(&method, message.params) {
                    Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
                    None => json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": { "code": METHOD_NOT_FOUND, "message": format!("unhandled method {method}") },
                    }),
                };
                self.send(reply).await
            }
            (None, Some(method)) => {
                self.notification(&method, message.params);
                Ok(())
            }
            (Some(id), None) => {
                let waiting = id.as_i64().and_then(|id| self.pending.remove(&id));
                if let Some(waiting) = waiting {
                    let result = match message.error {
                        Some(e) => Err(Error::Response {
                            code: e.code,
                            message: e.message,
                        }),
                        None => Ok(message.result.unwrap_or(Value::Null)),
                    };
                    let _ = waiting.send(result);
                }
                Ok(())
            }
            (None, None) => Ok(()),
        }
    }

    /// The answer to a request from the server, or `None` for "method not
    /// found".
    fn server_request(&mut self, method: &str, params: Value) -> Option<Value> {
        match method {
            "workspace/configuration" => {
                let items = serde_json::from_value::<ConfigurationParams>(params)
                    .map(|p| p.items)
                    .unwrap_or_default();
                let values = items
                    .iter()
                    .map(|item| {
                        configuration(self.initialization.as_ref(), item.section.as_deref())
                    })
                    .collect();
                Some(Value::Array(values))
            }
            "client/registerCapability" => {
                if let Ok(params) = serde_json::from_value::<RegistrationParams>(params) {
                    let added: Vec<_> = params
                        .registrations
                        .into_iter()
                        .filter(|r| r.method == DIAGNOSTIC_METHOD)
                        .collect();
                    if !added.is_empty() {
                        self.store.send_modify(|store| {
                            for registration in added {
                                store
                                    .registrations
                                    .insert(registration.id.clone(), registration);
                            }
                            store.registrations_changed += 1;
                        });
                    }
                }
                Some(Value::Null)
            }
            "client/unregisterCapability" => {
                if let Ok(params) = serde_json::from_value::<UnregistrationParams>(params) {
                    let removed: Vec<_> = params
                        .unregisterations
                        .into_iter()
                        .filter(|r| r.method == DIAGNOSTIC_METHOD)
                        .map(|r| r.id)
                        .collect();
                    if !removed.is_empty() {
                        self.store.send_modify(|store| {
                            for id in &removed {
                                store.registrations.remove(id);
                            }
                            store.registrations_changed += 1;
                        });
                    }
                }
                Some(Value::Null)
            }
            "workspace/workspaceFolders" => Some(json!([
                { "name": "workspace", "uri": uri::from_path(&self.root) }
            ])),
            "window/workDoneProgress/create" | "workspace/diagnostic/refresh" => Some(Value::Null),
            _ => None,
        }
    }

    fn notification(&mut self, method: &str, params: Value) {
        if method != "textDocument/publishDiagnostics" {
            return;
        }
        let Ok(params) = serde_json::from_value::<PublishDiagnosticsParams>(params) else {
            return;
        };
        let Some(path) = uri::to_path(&params.uri) else {
            return;
        };
        self.store.send_modify(|store| {
            store
                .published
                .insert(path.clone(), (Instant::now(), params.version));
            store.push.insert(path, params.diagnostics);
        });
    }
}

/// The `section` (dotted) of the initialization options, or `null`.
fn configuration(settings: Option<&Value>, section: Option<&str>) -> Value {
    let Some(section) = section else {
        return settings.cloned().unwrap_or(Value::Null);
    };
    settings
        .and_then(|settings| {
            section
                .split('.')
                .try_fold(settings, |value, key| value.get(key))
        })
        .cloned()
        .unwrap_or(Value::Null)
}

/// Where the text ends, in LSP's UTF-16 columns.
fn end_of(text: &str) -> Position {
    let last = text.rsplit(['\n', '\r']).next().unwrap_or("");
    let breaks = text.replace("\r\n", "\n").matches(['\n', '\r']).count();
    Position {
        line: u32::try_from(breaks).unwrap_or(u32::MAX),
        character: u32::try_from(last.encode_utf16().count()).unwrap_or(u32::MAX),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn end_counts_every_line_break_style() {
        assert_eq!(
            end_of(""),
            Position {
                line: 0,
                character: 0
            }
        );
        assert_eq!(
            end_of("ab\ncd"),
            Position {
                line: 1,
                character: 2
            }
        );
        assert_eq!(
            end_of("a\r\nb\rc\n"),
            Position {
                line: 3,
                character: 0
            }
        );
        assert_eq!(
            end_of("x\n😀"),
            Position {
                line: 1,
                character: 2
            }
        );
    }

    #[test]
    fn configuration_walks_dotted_sections() {
        let settings = json!({ "a": { "b": 1 } });
        assert_eq!(configuration(Some(&settings), Some("a.b")), json!(1));
        assert_eq!(configuration(Some(&settings), Some("a.c")), Value::Null);
        assert_eq!(configuration(Some(&settings), None), settings);
        assert_eq!(configuration(None, Some("a")), Value::Null);
    }
}
