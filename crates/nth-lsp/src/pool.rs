//! The process-wide set of language server clients, started lazily: one per
//! (server id, project root), the first time a file there is touched.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use futures::{
    FutureExt,
    future::{BoxFuture, Shared, join_all},
};
use tokio::{sync::watch, time::Instant};
use tokio_util::sync::CancellationToken;

use crate::{
    client::{Client, Error, Stderr},
    config::LspConfig,
    server::{self, Scope, Server},
    types::Diagnostic,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerStatus {
    pub id: String,
    pub root: PathBuf,
    pub state: ServerState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerState {
    Starting,
    Connected,
    /// It failed to start or died, and is not tried again.
    Broken(String),
}

/// What [`Lsp::servers_for`] reports about one server.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerInfo {
    pub id: String,
    pub extensions: Vec<String>,
    /// The program that would run, when one is on PATH.
    pub program: Option<PathBuf>,
    /// The root it would run in for a file in the directory asked about.
    pub root: Option<PathBuf>,
}

type Key = (String, PathBuf);
/// One client's start, shared by everyone who touches its (id, root). The
/// first runs it in a task of its own, since a cancelled turn drops its tool
/// futures and must not take a server that is still starting down with it;
/// the rest wait on the same outcome. `None` means it could not start, for
/// good, which is opencode's `broken` set.
type Slot = Shared<BoxFuture<'static, Option<Arc<Client>>>>;

/// Cheap to clone; every clone shares the same clients. Dropping the last
/// one stops them all; [`Lsp::shutdown`] first asks them to leave.
#[derive(Clone)]
pub struct Lsp {
    inner: Arc<Inner>,
}

struct Inner {
    servers: Arc<[Server]>,
    clients: Mutex<HashMap<Key, Slot>>,
    status: watch::Sender<Vec<ServerStatus>>,
    cancel: CancellationToken,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Lsp {
    pub fn new(config: &LspConfig) -> Self {
        Self::with_servers(server::registry(config))
    }

    fn with_servers(servers: Vec<Server>) -> Self {
        Self {
            inner: Arc::new(Inner {
                servers: servers.into(),
                clients: Mutex::new(HashMap::new()),
                status: watch::Sender::new(Vec::new()),
                cancel: CancellationToken::new(),
            }),
        }
    }

    /// Every client nth has tried to start, as it changes.
    pub fn status(&self) -> watch::Receiver<Vec<ServerStatus>> {
        self.inner.status.subscribe()
    }

    /// The servers that would handle files in `cwd`. Blocking.
    pub fn servers_for(&self, cwd: &Path) -> Vec<ServerInfo> {
        // Any file name will do: roots depend only on the directory.
        let probe = cwd.join("_");
        let scope = Scope::of(&probe);
        self.inner
            .servers
            .iter()
            .map(|server| ServerInfo {
                id: server.id.clone(),
                extensions: server.extensions.clone(),
                program: server.program().map(|(program, _)| program),
                root: server.root(&probe, &scope),
            })
            .collect()
    }

    /// Tells every server for `path` about its current text, starting them
    /// if needed. With `wait`, gives them a moment to report on it. Returns
    /// all the diagnostics those servers know of, for this file and others.
    pub async fn touch(&self, path: &Path, wait: bool) -> BTreeMap<PathBuf, Vec<Diagnostic>> {
        let path = match std::path::absolute(path) {
            Ok(path) => path,
            Err(_) => return BTreeMap::new(),
        };
        let clients = self.clients_for(&path).await;
        let alive = join_all(clients.iter().map(|client| async {
            let after = Instant::now();
            match client.open_or_change(&path).await {
                Ok(version) if wait => client.wait_for_diagnostics(&path, version, after).await,
                Ok(_) => {}
                Err(Error::Closed) => {
                    let reason = client.stderr().explain("exited");
                    self.set_state(client.id(), client.root(), ServerState::Broken(reason));
                    return false;
                }
                // The file vanished or is not text: nothing to report.
                Err(_) => {}
            }
            true
        }))
        .await;

        let mut all: BTreeMap<PathBuf, Vec<Diagnostic>> = BTreeMap::new();
        // A dead server's last word is not worth repeating on every write.
        for (client, _) in clients.iter().zip(alive).filter(|(_, alive)| *alive) {
            for (file, diagnostics) in client.diagnostics() {
                all.entry(file).or_default().extend(diagnostics);
            }
        }
        all
    }

    /// Asks every running server to shut down and exit, then stops their
    /// tasks, which kills any still around. For the end of a run: dropping
    /// the last `Lsp` only does the killing.
    pub async fn shutdown(&self) {
        let clients: Vec<Arc<Client>> = {
            let clients = self
                .inner
                .clients
                .lock()
                .expect("lsp client map lock poisoned");
            clients
                .values()
                .filter_map(|slot| slot.peek().cloned().flatten())
                .collect()
        };
        join_all(clients.iter().map(|client| client.shutdown())).await;
        self.inner.cancel.cancel();
    }

    async fn clients_for(&self, path: &Path) -> Vec<Arc<Client>> {
        let servers = Arc::clone(&self.inner.servers);
        let file = path.to_path_buf();
        let matches = tokio::task::spawn_blocking(move || {
            let scope = Scope::of(&file);
            servers
                .iter()
                .filter(|server| server.handles(&file))
                .filter_map(|server| {
                    let root = server.root(&file, &scope)?;
                    Some((server.clone(), root))
                })
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();

        let started = join_all(
            matches
                .into_iter()
                .map(|(server, root)| self.client(server, root)),
        )
        .await;
        started.into_iter().flatten().collect()
    }

    async fn client(&self, server: Server, root: PathBuf) -> Option<Arc<Client>> {
        let slot = {
            let mut clients = self
                .inner
                .clients
                .lock()
                .expect("lsp client map lock poisoned");
            clients
                .entry((server.id.clone(), root.clone()))
                .or_insert_with(|| {
                    let lsp = self.clone();
                    let started = tokio::spawn(async move { lsp.start(server, root).await });
                    // A start that panicked counts as a failed one.
                    async move { started.await.unwrap_or(None) }
                        .boxed()
                        .shared()
                })
                .clone()
        };
        slot.await
    }

    async fn start(&self, server: Server, root: PathBuf) -> Option<Arc<Client>> {
        let launch = {
            let (server, root) = (server.clone(), root.clone());
            tokio::task::spawn_blocking(move || server.launch(&root))
                .await
                .ok()??
        };
        self.set_state(&server.id, &root, ServerState::Starting);
        let stderr = Stderr::default();
        let cancel = self.inner.cancel.child_token();
        match Client::spawn(&server.id, launch, cancel, stderr.clone()).await {
            Ok(client) => {
                self.set_state(&server.id, &root, ServerState::Connected);
                Some(Arc::new(client))
            }
            Err(e) => {
                let reason = stderr.explain(&e.to_string());
                self.set_state(&server.id, &root, ServerState::Broken(reason));
                None
            }
        }
    }

    /// Only a change wakes the watchers: a broken server is reported again
    /// on every touch, and the front-end need not redraw for that.
    fn set_state(&self, id: &str, root: &Path, state: ServerState) {
        self.inner.status.send_if_modified(|all| {
            match all.iter_mut().find(|s| s.id == id && s.root == root) {
                Some(status) if status.state == state => false,
                Some(status) => {
                    status.state = state;
                    true
                }
                None => {
                    all.push(ServerStatus {
                        id: id.to_string(),
                        root: root.to_path_buf(),
                        state,
                    });
                    true
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::config::ServerConfig;

    fn config_with(id: &str, command: &[&str], extensions: &[&str]) -> LspConfig {
        let mut config = LspConfig {
            enabled: true,
            servers: BTreeMap::new(),
        };
        config.servers.insert(
            id.into(),
            ServerConfig {
                command: command.iter().map(|s| s.to_string()).collect(),
                extensions: extensions.iter().map(|s| s.to_string()).collect(),
                ..Default::default()
            },
        );
        config
    }

    #[tokio::test]
    async fn a_server_that_fails_to_start_is_broken_for_good() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let file = tmp.path().join("a.nthtest");
        std::fs::write(&file, "x").unwrap();
        // `true` exits at once, so the handshake fails.
        let lsp = Lsp::new(&config_with("fake", &["true"], &[".nthtest"]));
        let status = lsp.status();

        assert!(lsp.touch(&file, true).await.is_empty());
        let after_first = status.borrow().clone();
        assert_eq!(after_first.len(), 1);
        assert_eq!(after_first[0].id, "fake");
        assert_eq!(after_first[0].root, tmp.path());
        assert!(matches!(after_first[0].state, ServerState::Broken(_)));

        // Not retried: the status does not go back to Starting.
        lsp.touch(&file, false).await;
        assert_eq!(*status.borrow(), after_first);
    }

    /// A language server as a shell script: it answers `initialize`, and
    /// on every `didOpen` publishes one error for that file, versioned.
    /// With `die` as its argument it exits right after publishing. What it
    /// writes to stderr on start shows up in the broken-state text.
    const SCRIPTED_SERVER: &str = r##"#!/bin/sh
echo "scripted server here" >&2
message() {
    length=0
    while IFS= read -r line; do
        line=$(printf '%s' "$line" | tr -d '\r')
        case "$line" in
            "") break ;;
            Content-Length:*) length=${line#Content-Length: } ;;
        esac
    done
    [ "$length" -gt 0 ] || return 1
    # One byte at a time: BSD head buffers and would eat the next message.
    body=$(dd bs=1 count="$length" 2>/dev/null)
}
send() {
    printf 'Content-Length: %d\r\n\r\n%s' "${#1}" "$1"
}
while message; do
    case "$body" in
        *'"method":"initialize"'*)
            id=$(printf '%s' "$body" | sed 's/.*"id":\([0-9]*\).*/\1/')
            send "{\"jsonrpc\":\"2.0\",\"id\":$id,\"result\":{\"capabilities\":{}}}"
            ;;
        *'"method":"textDocument/didOpen"'*)
            uri=$(printf '%s' "$body" | sed 's/.*"uri":"\([^"]*\)".*/\1/')
            range='{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}'
            item="{\"range\":$range,\"severity\":1,\"message\":\"boom\"}"
            params="{\"uri\":\"$uri\",\"version\":0,\"diagnostics\":[$item]}"
            send "{\"jsonrpc\":\"2.0\",\"method\":\"textDocument/publishDiagnostics\",\"params\":$params}"
            [ "$1" = die ] && exit 0
            ;;
    esac
done
"##;

    /// A checkout with one `.nthtest` file, served by the scripted server
    /// started with `args`.
    fn scripted(args: &[&str]) -> (tempfile::TempDir, PathBuf, Lsp) {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let file = tmp.path().join("a.nthtest");
        std::fs::write(&file, "x").unwrap();
        let script = tmp.path().join("server.sh");
        std::fs::write(&script, SCRIPTED_SERVER).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut command = vec![script.to_str().unwrap()];
        command.extend(args);
        let lsp = Lsp::new(&config_with("fake", &command, &[".nthtest"]));
        (tmp, file, lsp)
    }

    fn state_of(status: &[ServerStatus]) -> Option<ServerState> {
        status.first().map(|s| s.state.clone())
    }

    #[tokio::test]
    async fn a_touch_given_up_on_leaves_the_server_starting() {
        let (_tmp, file, lsp) = scripted(&[]);
        let mut status = lsp.status();

        // The turn is cancelled as soon as the server is being started.
        let touch = lsp.touch(&file, true);
        tokio::select! {
            _ = touch => panic!("the touch finished before the server was started"),
            _ = status.wait_for(|s| state_of(s) == Some(ServerState::Starting)) => {}
        }

        // The start goes on without it...
        tokio::time::timeout(
            Duration::from_secs(10),
            status.wait_for(|s| state_of(s) == Some(ServerState::Connected)),
        )
        .await
        .expect("the server came up")
        .unwrap();
        // ... and the next touch uses the client it made.
        let diagnostics = lsp.touch(&file, true).await;
        assert_eq!(diagnostics[&file][0].message, "boom");
    }

    #[tokio::test]
    async fn a_server_that_exits_takes_its_diagnostics_with_it() {
        let (_tmp, file, lsp) = scripted(&["die"]);

        let first = lsp.touch(&file, true).await;
        assert_eq!(first[&file][0].message, "boom");

        // It exited after publishing: the next touch finds it gone, and its
        // last report is not repeated.
        let again = lsp.touch(&file, true).await;
        assert!(again.is_empty(), "{again:?}");
        let status = lsp.status().borrow().clone();
        match &status[0].state {
            ServerState::Broken(reason) => {
                assert_eq!(reason, "exited: scripted server here", "{reason}")
            }
            other => panic!("{other:?}"),
        }
        assert!(lsp.touch(&file, true).await.is_empty());
    }

    #[tokio::test]
    async fn a_server_that_fails_to_start_says_why() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let file = tmp.path().join("a.nthtest");
        std::fs::write(&file, "x").unwrap();
        let command = ["sh", "-c", "echo 'no such option' >&2; exit 2"];
        let lsp = Lsp::new(&config_with("fake", &command, &[".nthtest"]));

        assert!(lsp.touch(&file, true).await.is_empty());
        let status = lsp.status().borrow().clone();
        match &status[0].state {
            ServerState::Broken(reason) => {
                assert!(reason.ends_with(": no such option"), "{reason}")
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn servers_not_on_path_are_skipped_quietly() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.nthtest");
        std::fs::write(&file, "x").unwrap();
        let lsp = Lsp::new(&config_with(
            "ghost",
            &["no-such-server-nth"],
            &[".nthtest"],
        ));
        assert!(lsp.touch(&file, true).await.is_empty());
        assert!(lsp.status().borrow().is_empty());
    }

    #[test]
    fn servers_for_lists_programs_and_roots() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".git")).unwrap();
        let lsp = Lsp::new(&config_with("shell", &["sh"], &[".nthtest"]));
        let servers = lsp.servers_for(tmp.path());
        let shell = servers.iter().find(|s| s.id == "shell").unwrap();
        assert!(shell.program.is_some());
        assert_eq!(shell.root.as_deref(), Some(tmp.path()));
        assert!(servers.iter().any(|s| s.id == "rust"));
    }
}
